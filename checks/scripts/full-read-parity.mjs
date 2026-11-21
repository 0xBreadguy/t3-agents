// The full reader against the original, on databases built to break a wrong
// rewrite of its queries. Each database ties hundreds of rows on the sort keys
// exactly where the caps cut (3,000 items, 8,000 events), spreads them across
// threads, runs, a subagent's run and a runless thread, gives them row ids that
// disagree with both insertion order and index order, and puts a tied group of
// events exactly on the 3-hour boundary so that moving the clock by a
// millisecond moves rows in and out of the window. Todo lists tie on their
// timestamp within and across runs. Items reach a subagent through a chain of
// nodes, so the node map decides where they land.
//
// Every state is read by the TypeScript Source and by
// `t3-agents-core --db PATH --now-ms N --dump-snapshot` at three clocks, then
// compared whole (null fields stripped, root path ignored, as scale-parity.mjs
// does). For the displaced malformed item, compare Rust's successful read
// with the original's successful read after deleting that one row.
//
// Variants: T3's schema; the same without any secondary index on turn items or
// nodes (table scans); with extra covering indexes (different plans); nodes
// without a primary key and a duplicated node id (last row wins, so order
// matters); and a malformed payload in an item row that enters the items
// sorter early and is displaced later.
//
//   node checks/scripts/full-read-parity.mjs [variant...]
//   T3_NATIVE_BINARY=path node checks/scripts/full-read-parity.mjs
//
// Synthetic data only, written under checks/generated/full-read-parity/.
import { createHash } from 'node:crypto'
import { copyFileSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'
import { DatabaseSync } from 'node:sqlite'
import { Source } from '../../oracle/src/source.ts'
// Opt-in: with T3_RECORD set, export-full-read.ts turns this run into the recorded fixture.
import {canonicalDigest} from './golden.ts'
import {enabled as recording, recordAsset, recordCase} from './record.ts'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const binary = resolve(process.env.T3_NATIVE_BINARY ?? join(root, '..', 'target/release/t3-agents-core'))
const out = join(root, 'generated/full-read-parity')
const SCHEMA = readFileSync(join(root, 'fixtures/t3-schema.sql'), 'utf8')
const P = 'orchestration_v2_projection_'
const NOW = 1794484800000
const H = 3600_000
const SINCE = NOW - 3 * H
const CLOCKS = [NOW, NOW + 1, NOW + 2]
const iso = (ms) => new Date(ms).toISOString()

function rng(seed) {
  let a = seed >>> 0
  return () => {
    a = (a + 0x6d2b79f5) >>> 0
    let t = a
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}
function shuffle(list, r) {
  for (let i = list.length - 1; i > 0; i--) {
    const j = Math.floor(r() * (i + 1))
    ;[list[i], list[j]] = [list[j], list[i]]
  }
  return list
}

// Thread ids sort differently from the order they are created in.
const THREADS = ['t-z', 't-a', 't-m', 't-q', 't-runless']
// Each thread's runs, oldest first. The last is the latest run.
const RUNS = { 't-z': ['r-z1', 'r-z2'], 't-a': ['r-a1'], 't-m': ['r-m1', 'r-m2'], 't-q': ['r-q1'] }
const LATEST = { 't-z': 'r-z2', 't-a': 'r-a1', 't-m': 'r-m2', 't-q': 'r-q1' }
// A subagent on t-z owns the older run r-z1; items reach it through nodes.
const SUB = 'sub-k'
// Where item rows go: [thread, run, node]. Rows on r-z1 without a node are
// candidates for the cap but attach to no agent; they still use up cap slots.
const LANES = [
  ['t-z', 'r-z2', null], ['t-a', 'r-a1', null], ['t-m', 'r-m2', null], ['t-q', 'r-q1', null],
  ['t-z', 'r-z1', SUB], ['t-z', 'r-z1', 'n-child-1'], ['t-z', 'r-z1', 'n-child-3'], ['t-z', 'r-z1', 'n-dup-a'],
  ['t-z', 'r-z1', 'n-dup-b'],
  ['t-z', 'r-z1', null], ['t-runless', null, null],
]
const TOOLS = ['command_execution', 'dynamic_tool', 'file_change', 'web_search', 'reasoning', 'assistant_message', 'subagent']

function payloadFor(type, label, started) {
  const p = { startedAt: iso(started), title: label }
  if (type === 'command_execution') Object.assign(p, { input: `run ${label}`, exitCode: label.length % 2 })
  if (type === 'dynamic_tool') Object.assign(p, { toolName: ['Read', 'Edit', 'WebFetch', 'Task', 'mcp'][label.length % 5] })
  if (type === 'file_change') Object.assign(p, { fileName: `src/${label}.rs` })
  if (type === 'reasoning' || type === 'assistant_message') delete p.title
  return JSON.stringify(p)
}

class Builder {
  constructor(path, variant) {
    this.variant = variant
    this.r = rng(20261115)
    this.db = new DatabaseSync(path)
    this.db.exec('PRAGMA journal_mode=WAL')
    let schema = SCHEMA
    if (variant === 'nopk') schema = schema.replace(`CREATE TABLE ${P}nodes ( node_id TEXT PRIMARY KEY,`, `CREATE TABLE ${P}nodes ( node_id TEXT,`)
    if (schema === SCHEMA && variant === 'nopk') throw new Error('nodes primary key not found in the schema fixture')
    this.db.exec(schema)
    if (variant === 'noindex') {
      for (const { name } of this.db.prepare(`SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL AND tbl_name IN ('${P}turn_items','${P}nodes')`).all()) {
        this.db.exec(`DROP INDEX "${name}"`)
      }
    }
    if (variant === 'covering') {
      this.db.exec(`CREATE INDEX x_items_cover ON ${P}turn_items(run_id, type, updated_at, ordinal)`)
      this.db.exec(`CREATE INDEX x_plans_cover ON ${P}turn_items(type, run_id, updated_at)`)
      this.db.exec(`CREATE INDEX x_nodes_cover ON ${P}nodes(thread_id, node_id, parent_node_id, run_id)`)
    }
    // Row ids come from a shuffled range twice the size, so later inserts can land in gaps below and above.
    this.free = shuffle(Array.from({ length: 60000 }, (_, i) => i + 1), this.r)
    this.seq = 0
    this.items = []
    // While recording, every write a mutation makes is logged so the fixture can rebuild the
    // database round by round instead of storing a dump for each round.
    this.log = recording ? [] : null
  }
  rowid() {
    return this.free.pop()
  }
  base() {
    const db = this.db
    db.exec('BEGIN')
    db.prepare(`INSERT INTO projection_projects (project_id,title,workspace_root,scripts_json,created_at,updated_at) VALUES ('p-1','Lab','/lab','[]',?,?)`).run(iso(NOW - 9 * H), iso(NOW - 9 * H))
    for (const t of THREADS) {
      const payload = { id: t, projectId: 'p-1', title: `thread ${t}`, updatedAt: iso(NOW - 5 * H) }
      db.prepare(`INSERT INTO ${P}threads (thread_id,project_id,title,default_provider,runtime_mode,interaction_mode,created_at,updated_at,payload_json) VALUES (?,?,?,?,?,?,?,?,?)`)
        .run(t, 'p-1', payload.title, 'codex', 'full', 'default', iso(NOW - 9 * H), payload.updatedAt, JSON.stringify(payload))
    }
    for (const [t, runs] of Object.entries(RUNS)) {
      runs.forEach((run, i) => {
        const status = run === LATEST[t] ? 'running' : 'completed'
        const payload = { id: run, threadId: t, status, startedAt: iso(NOW - 8 * H + i), requestedAt: iso(NOW - 8 * H + i) }
        db.prepare(`INSERT INTO ${P}runs (run_id,thread_id,ordinal,provider,status,requested_at,completed_at,payload_json) VALUES (?,?,?,?,?,?,?,?)`)
          .run(run, t, i + 1, 'codex', status, payload.requestedAt, status === 'completed' ? iso(NOW - 4 * H) : null, JSON.stringify(payload))
      })
    }
    const sub = { id: SUB, threadId: 't-z', parentNodeId: 'n-root-z', runId: 'r-z1', status: 'running', title: 'tied subagent', startedAt: iso(NOW - 7 * H), updatedAt: iso(NOW - 7 * H) }
    db.prepare(`INSERT INTO ${P}subagents (subagent_id,thread_id,run_id,parent_node_id,provider,origin,status,updated_at,payload_json) VALUES (?,?,?,?,?,?,?,?,?)`)
      .run(SUB, 't-z', 'r-z1', 'n-root-z', 'codex', 'native', 'running', sub.updatedAt, JSON.stringify(sub))
    // n-child-1 → sub-k; n-child-2 → n-child-1 (two hops); n-child-3 → the thread root.
    const nodes = [['n-root-z', null, 'r-z2'], ['n-child-1', SUB, 'r-z1'], ['n-child-2', 'n-child-1', 'r-z1'], ['n-child-3', 'n-root-z', 'r-z1']]
    // Without a primary key a node id can repeat, and the last row read wins. Each pair
    // differs in the settled-leaf index's expression (kind tool_call, settled, with a run)
    // so that index's order and row-id order disagree: in pair a the lower row id sorts
    // last in the index, in pair b the higher one does. Items reach the subagent only
    // through the row whose parent is sub-k.
    const dup = this.free.splice(-4).sort((x, y) => x - y)
    const pairs = this.variant === 'nopk'
      ? [[dup[0], 'n-dup-a', SUB, 'tool_call'], [dup[1], 'n-dup-a', 'n-root-z', 'tool'],
        [dup[2], 'n-dup-b', SUB, 'tool'], [dup[3], 'n-dup-b', 'n-root-z', 'tool_call']]
      : [[dup[0], 'n-dup-a', SUB, 'tool_call'], [dup[3], 'n-dup-b', 'n-root-z', 'tool_call']]
    for (const [rowid, id, parent, kind] of pairs) {
      db.prepare(`INSERT INTO ${P}nodes (rowid,node_id,thread_id,run_id,parent_node_id,root_node_id,kind,status,payload_json) VALUES (?,?,?,?,?,?,?,?,?)`)
        .run(rowid, id, 't-z', 'r-z1', parent, 'n-root-z', kind, 'completed', '{}')
    }
    // Filler nodes on every thread, so the node read has rows to skip and to keep.
    for (const t of THREADS) for (let i = 0; i < 300; i++) nodes.push([`n-${t}-${i}`, i ? `n-${t}-${i - 1}` : null, LATEST[t] ?? null, t])
    for (const [id, parent, run, thread] of shuffle(nodes, this.r)) {
      db.prepare(`INSERT INTO ${P}nodes (rowid,node_id,thread_id,run_id,parent_node_id,root_node_id,kind,status,payload_json) VALUES (?,?,?,?,?,?,?,?,?)`)
        .run(this.rowid(), id, thread ?? 't-z', run, parent, 'n-root-z', 'tool', 'completed', '{}')
    }
    const rows = []
    const add = (n, at, ordinal, opts = {}) => {
      for (let i = 0; i < n; i++) {
        const [thread, run, node] = opts.lane ?? LANES[Math.floor(this.r() * LANES.length)]
        rows.push({ thread, run, node, at: typeof at === 'function' ? at(i) : at, ordinal: typeof ordinal === 'function' ? ordinal(i) : ordinal,
          type: opts.type ?? TOOLS[Math.floor(this.r() * TOOLS.length)], status: opts.status ?? (this.r() < 0.12 ? 'failed' : 'completed') })
      }
    }
    const U = NOW - 2 * H
    // Items cap: 200 newest, 2,200 more in tied groups of ten (ties away from the cut),
    // then 400 at (U, 9) and 800 at (U, 5): the cut keeps 200 of the second group.
    add(200, (i) => NOW - 600_000 + i * 997, (i) => i)
    add(2200, (i) => NOW - H + Math.floor(i / 10) * 1000, (i) => 100 + (i % 3))
    add(400, U, 9)
    add(800, U, 5)
    add(200, (i) => NOW - 2.5 * H - i, 1)
    add(200, (i) => NOW - 4 * H - i, 1) // outside the event window
    // Events cap: non-tool rows only events see. 2,000 newer, 300 one millisecond inside
    // the boundary, 3,000 exactly on it (the cut lands inside), 200 one millisecond outside.
    add(2000, (i) => NOW - 50 * 60_000 + i, 0, { type: 'file_search' })
    add(300, SINCE + 1, 0, { type: 'user_message' })
    add(3000, SINCE, 0, { type: 'file_search' })
    add(200, SINCE - 1, 0, { type: 'file_search' })
    // Todo lists: tied within and across runs, ordinals and row ids disagreeing.
    for (const run of ['r-z2', 'r-a1', 'r-m2', 'r-q1', 'r-z1', 'r-m1']) {
      const lane = LANES.find((l) => l[1] === run) ?? ['t-m', run, null]
      for (let k = 0; k < 4; k++) add(1, NOW - 30 * 60_000, 7 - (k % 2), { lane: run === 'r-z1' ? ['t-z', 'r-z1', SUB] : lane, type: 'todo_list', status: 'running' })
    }
    add(1, NOW - 20 * 60_000, 3, { lane: ['t-a', 'r-a1', null], type: 'todo_list', status: 'running' })
    // Tied todo lists: within each run the higher ordinal gets the lower row ids, so
    // row-id order and the run index's (ordinal, row id) order pick different winners.
    const todos = rows.filter((x) => x.type === 'todo_list').sort((x, y) => String(x.run).localeCompare(String(y.run)) || y.ordinal - x.ordinal)
    const pool = this.free.splice(-todos.length).sort((x, y) => x - y)
    todos.forEach((row, i) => { row.rowid = pool[i] })
    for (const row of shuffle(rows, this.r)) this.insert(row, row.rowid ?? this.rowid())
    if (this.variant === 'malformed') {
      // Visited first by the item plan (lowest run id, lowest ordinal), oldest, so it enters
      // the sorter while it has room and is displaced later.
      db.prepare(`INSERT INTO ${P}turn_items (rowid,turn_item_id,thread_id,run_id,node_id,ordinal,type,status,updated_at,payload_json) VALUES (?,?,?,?,?,?,?,?,?,?)`)
        .run(this.rowid(), 'i-malformed', 't-a', 'r-a1', null, -5, 'command_execution', 'completed', iso(NOW - 5 * H), '{"startedAt": "x", "title": ')
    }
    db.exec('COMMIT')
  }
  insert(row, rowid = this.rowid()) {
    const n = this.seq++
    const label = `${row.type}-${row.thread}-${row.run ?? 'none'}-${n}`
    let payload = payloadFor(row.type, label, row.at - 1000 - n)
    if (row.type === 'todo_list') payload = JSON.stringify({ startedAt: iso(row.at), steps: [{ text: `step of ${label}`, status: 'in_progress' }, { text: `then ${n}`, status: 'pending' }] })
    this.db.prepare(`INSERT INTO ${P}turn_items (rowid,turn_item_id,thread_id,run_id,node_id,provider_thread_id,ordinal,type,status,updated_at,payload_json) VALUES (?,?,?,?,?,?,?,?,?,?,?)`)
      .run(rowid, `i-${n}`, row.thread, row.run, row.node, null, row.ordinal, row.type, row.status, iso(row.at), payload)
    this.items.push({ rowid, ...row })
  }
  // Moves the cut and the tie groups: deletes inside and above the cut, inserts into the
  // tied group with row ids below and above every existing one, retimes and re-ranks rows.
  mutate(round) {
    const real = this.db
    if (this.log) {
      const log = this.log
      log.length = 0
      this.db = {
        exec: (sql) => { log.push({ sql }); real.exec(sql) },
        prepare: (sql) => { const statement = real.prepare(sql); return { run: (...args) => { log.push({ sql, args }); return statement.run(...args) } } },
      }
    }
    const db = this.db
    const U = NOW - 2 * H
    const pick = (filter, n) => shuffle(this.items.filter(filter), this.r).slice(0, n)
    db.exec('BEGIN')
    if (round === 1) {
      for (const row of pick((x) => x.at >= NOW - H && x.at < NOW - H + 220_000 && x.type !== 'todo_list', 150)) db.prepare(`DELETE FROM ${P}turn_items WHERE rowid=?`).run(row.rowid)
      const low = this.free.filter((id) => id < 2000).slice(0, 20)
      const high = [70001, 70002, 70003, 70004, 70005, 70006, 70007, 70008, 70009, 70010, 70011, 70012, 70013, 70014, 70015, 70016, 70017, 70018, 70019, 70020]
      for (const id of [...low, ...high]) {
        this.free = this.free.filter((x) => x !== id)
        const [thread, run, node] = LANES[id % LANES.length]
        this.insert({ thread, run, node, at: U, ordinal: 5, type: TOOLS[id % TOOLS.length], status: 'completed' }, id)
      }
      for (const row of pick((x) => x.at === U && x.ordinal === 9, 30)) db.prepare(`UPDATE ${P}turn_items SET ordinal=5 WHERE rowid=?`).run(row.rowid)
      for (let i = 0; i < 500; i++) this.insert({ thread: THREADS[i % THREADS.length], run: LATEST[THREADS[i % THREADS.length]] ?? null, node: null, at: SINCE, ordinal: 0, type: 'file_search', status: i % 7 ? 'completed' : 'failed' })
      const todo = pick((x) => x.type === 'todo_list' && x.run === 'r-z2', 1)[0]
      db.prepare(`UPDATE ${P}turn_items SET updated_at=? WHERE rowid=?`).run(iso(NOW - 10 * 60_000), todo.rowid)
      this.insert({ thread: 't-m', run: 'r-m2', node: null, at: NOW - 30 * 60_000, ordinal: 6, type: 'todo_list', status: 'running' }, this.free.filter((id) => id < 2000)[0])
    } else {
      for (const row of pick((x) => x.at >= NOW - 600_000 && x.type !== 'todo_list', 200)) db.prepare(`DELETE FROM ${P}turn_items WHERE rowid=?`).run(row.rowid)
      for (const row of pick((x) => x.at === U && x.ordinal === 5, 300)) db.prepare(`UPDATE ${P}turn_items SET updated_at=? WHERE rowid=?`).run(iso(U + 1), row.rowid)
      db.prepare(`UPDATE ${P}nodes SET parent_node_id=? WHERE node_id='n-child-3'`).run(SUB)
      db.prepare(`UPDATE ${P}nodes SET parent_node_id='n-root-z' WHERE node_id='n-child-1'`).run()
    }
    db.exec('COMMIT')
    this.db = real
  }
  close() {
    this.db.close()
  }
}

function normalized(value, top = false) {
  if (Array.isArray(value)) return value.map((v) => normalized(v))
  if (value && typeof value === 'object')
    return Object.fromEntries(Object.entries(value).filter(([k, v]) => v !== null && !(top && k === 'path')).map(([k, v]) => [k, normalized(v)]))
  return value
}
function diffs(a, b, path, found, cap) {
  if (found.length >= cap || Object.is(a, b)) return
  if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) found.push(`${path}: length ${a.length} != ${b.length}`)
    for (let i = 0; i < Math.min(a.length, b.length); i++) diffs(a[i], b[i], `${path}[${i}]`, found, cap)
    return
  }
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) diffs(a[k], b[k], path ? `${path}.${k}` : k, found, cap)
    return
  }
  found.push(`${path}: ${JSON.stringify(a)} != ${JSON.stringify(b)}`)
}

// What each state exercises, so a pass is not vacuous.
function coverage(path) {
  const db = new DatabaseSync(path, { readOnly: true })
  const one = (sql, ...args) => Object.values(db.prepare(sql).get(...args))[0]
  const runs = ['r-z2', 'r-a1', 'r-m2', 'r-q1', 'r-z1']
  const cand = `(run_id IN (${runs.map(() => '?').join(',')}) OR (run_id IS NULL AND thread_id='t-runless')) AND type IN (${TOOLS.map((t) => `'${t}'`).join(',')})`
  const cut = db.prepare(`SELECT updated_at, ordinal FROM ${P}turn_items WHERE ${cand} ORDER BY updated_at DESC, ordinal DESC LIMIT 1 OFFSET 2999`).get(...runs)
  const result = {
    items_candidates: one(`SELECT count(*) FROM ${P}turn_items WHERE ${cand}`, ...runs),
    items_tied_at_cut: cut ? one(`SELECT count(*) FROM ${P}turn_items WHERE ${cand} AND updated_at=? AND ordinal=?`, ...runs, cut.updated_at, cut.ordinal) : 0,
    events_tied_on_boundary: one(`SELECT count(*) FROM ${P}turn_items WHERE updated_at=? AND type NOT IN ('checkpoint','todo_list')`, iso(SINCE)),
    events_in_window: Object.fromEntries(CLOCKS.map((now) => [now - NOW, one(`SELECT count(*) FROM ${P}turn_items WHERE updated_at>=? AND type NOT IN ('checkpoint','todo_list')`, iso(now - 3 * H))])),
    todo_lists: one(`SELECT count(*) FROM ${P}turn_items WHERE type='todo_list'`),
  }
  db.close()
  return result
}

function compare(path, now, variant) {
  let expected, expectedError
  try {
    expected = normalized(new Source(path).snapshot(now), true)
  } catch (error) {
    expectedError = String(error?.message ?? error)
  }
  const result = spawnSync(binary, ['--db', path, '--now-ms', String(now), '--dump-snapshot'], { encoding: 'utf8', maxBuffer: 1 << 30, timeout: 300000 })
  // --dump-snapshot prints an empty snapshot stamped at 0 when the read fails.
  const dumped = result.status === 0 ? JSON.parse(result.stdout) : null
  const rustError = result.status !== 0 ? (result.stderr || 'exit ' + result.status).trim().slice(0, 200)
    : dumped.at === 0 && !dumped.agents.length ? 'read failed (empty snapshot at 0)' : undefined
  if (variant === 'malformed') {
    // Deliberate difference from oracle/: skipping payload_json parsing on displaced
    // rows outside the 3,000-item / 8,000-event cut means oracle/ fails on `i-malformed`
    // while Rust succeeds and matches oracle/'s snapshot once that out-of-cut row is removed.
    if (!/malformed JSON/i.test(expectedError ?? '') || rustError !== undefined) {
      return { result: { ok: false, original: expectedError ?? 'snapshot', rust: rustError ?? 'snapshot' }, snapshot: null }
    }
    const cleanPath = `${path}.clean`
    rmSync(cleanPath, { force: true })
    copyFileSync(path, cleanPath)
    let cleanExpected
    try {
      const cleanDb = new DatabaseSync(cleanPath)
      try {
        const { changes } = cleanDb.prepare(`DELETE FROM ${P}turn_items WHERE turn_item_id='i-malformed'`).run()
        if (changes !== 1) throw new Error('expected exactly one displaced malformed item')
      } finally {
        cleanDb.close()
      }
      cleanExpected = normalized(new Source(cleanPath).snapshot(now), true)
    } finally {
      rmSync(cleanPath, { force: true })
    }
    const actual = normalized(dumped, true)
    const found = []
    diffs(actual, cleanExpected, '', found, 20)
    const items = cleanExpected.agents.reduce((n, a) => n + a.items.length, 0)
    const events = cleanExpected.agents.reduce((n, a) => n + a.events.length, 0)
    const plans = cleanExpected.agents.filter((a) => a.plan).length
    return { result: { ok: found.length === 0, original: expectedError, agents: cleanExpected.agents.length, items, events, plans, diffs: found.slice(0, 8) }, snapshot: null }
  }
  if (expectedError !== undefined || rustError !== undefined) {
    const ok = expectedError !== undefined && rustError !== undefined
    return { result: { ok, original: expectedError ?? 'snapshot', rust: rustError ?? 'snapshot' }, snapshot: null }
  }
  const actual = normalized(dumped, true)
  const found = []
  diffs(actual, expected, '', found, 20)
  const items = expected.agents.reduce((n, a) => n + a.items.length, 0)
  const events = expected.agents.reduce((n, a) => n + a.events.length, 0)
  const plans = expected.agents.filter((a) => a.plan).length
  return { result: { ok: found.length === 0, agents: expected.agents.length, items, events, plans, diffs: found.slice(0, 8) }, snapshot: expected }
}

const VARIANTS = ['t3', 'noindex', 'covering', 'nopk', 'malformed']
const chosen = process.argv.slice(2).length ? process.argv.slice(2) : VARIANTS
const report = { binary, binary_sha256: createHash('sha256').update(readFileSync(binary)).digest('hex'), cases: [] }
let failures = 0
mkdirSync(out, { recursive: true })
for (const variant of chosen) {
  if (!VARIANTS.includes(variant)) throw new Error(`unknown variant ${variant}; expected ${VARIANTS.join(', ')}`)
  const dir = join(out, variant)
  rmSync(dir, { recursive: true, force: true })
  mkdirSync(dir, { recursive: true })
  const path = join(dir, 'state.sqlite')
  const builder = new Builder(path, variant)
  builder.base()
  const recordedInput = recording ? recordAsset(path, 'sql') : null
  const rounds = variant === 'malformed' ? [0] : [0, 1, 2]
  for (const round of rounds) {
    if (round) builder.mutate(round)
    builder.db.exec('PRAGMA wal_checkpoint(TRUNCATE)')
    const cover = coverage(path)
    for (const now of CLOCKS) {
      const { result: c, snapshot: expectedSnapshot } = compare(path, now, variant)
      report.cases.push({ variant, round, clock: `+${now - NOW}ms`, ...c, ...(now === NOW ? { coverage: cover } : {}) })
      if (recording) recordCase({ variant, round, clock: now - NOW, now, input: recordedInput, ok: c.ok,
        error: expectedSnapshot === null ? c.original : null, agents: c.agents ?? null, items: c.items ?? null, events: c.events ?? null,
        plans: c.plans ?? null, digest: expectedSnapshot === null ? null : canonicalDigest(expectedSnapshot),
        log: round ? builder.log.map((step) => ({ ...step })) : [] })
      if (!c.ok) failures++
    }
  }
  builder.close()
}
for (const c of report.cases) {
  const what = c.diffs ? `agents ${c.agents} items ${c.items} events ${c.events} plans ${c.plans}` : `original: ${c.original.slice(0, 60)} | rust: ${c.rust.slice(0, 60)}`
  console.log(`${c.ok ? 'ok  ' : 'FAIL'} ${c.variant.padEnd(9)} round ${c.round} ${c.clock.padEnd(6)} ${what}${c.coverage ? ' ' + JSON.stringify(c.coverage) : ''}`)
  for (const d of c.diffs ?? []) console.log('       ' + d)
}
console.log(`${report.cases.length - failures}/${report.cases.length} cases equal; binary ${report.binary_sha256.slice(0, 12)}`)
process.exit(failures ? 1 : 0)
