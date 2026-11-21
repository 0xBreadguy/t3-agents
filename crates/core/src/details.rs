//! Terminal-cell selected details, ported from oracle/src/view.ts.
//! This module deliberately keeps the original block layout, glyphs and density.
use crate::model::{self, Agent, Pace, State};
use crate::terminal::{self, Canvas, Palette, Rect};
use crate::ui::Prepared;
use serde_json::Value;

/// The same measurement the original's `textWidth` uses, so a truncation here agrees with
/// the one the screen draws: `fit` measures the raw string, `text` draws the cleaned one.
fn width(s: &str) -> i32 {
    terminal::width(s) as i32
}
fn fit(s: &str, max: i32) -> String {
    if max <= 0 {
        String::new()
    } else {
        terminal::fit(s, max as usize)
    }
}
fn wrap(s: &str, max: i32) -> Vec<String> {
    if max <= 0 {
        Vec::new()
    } else {
        terminal::wrap(s, max as usize)
    }
}
fn tokens(n: Option<f64>) -> String {
    n.map(terminal::tokens).unwrap_or_else(|| "--".into())
}
fn duration(s: Option<f64>) -> String {
    s.map(terminal::duration).unwrap_or_else(|| "--".into())
}
fn age(stamp: Option<&str>, now: f64) -> Option<f64> {
    model::ms(stamp).map(|t| (now - t) / 1000.)
}
fn coarse(sec: Option<f64>) -> String {
    sec.map(terminal::coarse).unwrap_or_else(|| "--".into())
}
fn short_model(m: &str) -> String {
    terminal::short_model(m)
}
fn mix(a: u32, b: u32, t: f64) -> u32 {
    terminal::mix(a, b, t)
}
fn ramp(p: &Palette, t: f64) -> u32 {
    terminal::ramp(p, t)
}
fn section(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    name: &str,
    right: &str,
    p: &Palette,
    right_color: u32,
) {
    terminal::section_with(c, x, y, w, name, p, right, right_color);
}
fn hbar(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    frac: f64,
    track: u32,
    color: impl Fn(i32, i32) -> u32,
) {
    terminal::hbar_with(
        c,
        x,
        y,
        w,
        frac,
        &terminal::BarOpts {
            track,
            color: &|i, n| color(i as i32, n as i32),
            marker: None,
            line: false,
        },
    );
}
fn context_bar(c: &mut Canvas, x: i32, y: i32, w: i32, r: f64, p: &Palette) {
    let floor = p.floor;
    hbar(c, x, y, w, r, p.track, |i, n| {
        let t = (i as f64 + 0.5) / n as f64;
        if t >= 0.9 {
            p.hot
        } else if t >= 0.75 {
            p.warn
        } else {
            ramp(p, floor + (floor.max(0.84) - floor) * (t / 0.75))
        }
    })
}
fn glyph(cat: &str) -> char {
    model::glyph(cat)
}
fn spinner(now: f64, calm: bool) -> char {
    if calm {
        '◉'
    } else {
        terminal::spinner(now)
    }
}
fn status(
    a: &Agent,
    now: f64,
    pc: Option<&Pace>,
    calm: bool,
    seen: bool,
    p: &Palette,
) -> (char, String, u32) {
    if model::needs_you(a) {
        return (
            if calm || seen || now % 1000. < 550. {
                '◆'
            } else {
                '◇'
            },
            "NEEDS YOU".into(),
            p.hot,
        );
    }
    if pc.is_some_and(|p| p.quiet) {
        return (
            '◌',
            "RUNNING".into(),
            if pc.is_some_and(|p| p.stalled) {
                p.warn
            } else {
                p.text2
            },
        );
    }
    match a.status.as_str() {
        "running" | "active" => (
            spinner(now, calm),
            "RUNNING".into(),
            if pc.is_some_and(|p| p.stalled) {
                p.warn
            } else {
                p.bright
            },
        ),
        "completed" => ('✓', "DONE".into(), p.dim),
        "interrupted" => ('⊘', "INTERRUPTED".into(), p.warn),
        "failed" | "error" => ('✗', "FAILED".into(), p.hot),
        // The statuses the original does not name show the value itself, uppercased:
        // `a.status.toUpperCase()`, so an unknown status reads `· UNKNOWN`, not IDLE.
        "starting" | "preparing" | "queued" | "pending" => ('◔', a.status.to_uppercase(), p.text2),
        _ => ('·', a.status.to_uppercase(), p.dim),
    }
}
fn ratio(a: &Agent) -> Option<f64> {
    model::ratio(a.context.used_tokens, a.context.max_tokens)
}
/// `Math.round(x) + "%"`, which rounds a tie up and prints `-0` as `0`.
fn percent(x: f64) -> String {
    format!("{}%", terminal::js_number(terminal::js_round(x)))
}
fn number(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.)
}
fn live_status(s: &str) -> bool {
    matches!(
        s,
        "running" | "starting" | "preparing" | "queued" | "waiting" | "pending" | "active"
    )
}

// Exact baseline digit cells come from terminal::draw_big, under the same face rule.
fn shadow_face(c: &Canvas) -> bool {
    let count = ((c.w as i32 + 1) / 19).clamp(2, 6);
    let tw = (c.w as i32 - (count - 1)) / count;
    c.h >= 44 && tw - 3 >= 28
}
fn big(c: &mut Canvas, x: i32, y: i32, s: &str, color: u32, p: &Palette) -> i32 {
    terminal::draw_big(c, x, y, s, color, mix(color, p.bg, 0.6), shadow_face(c))
}
fn big_rows(c: &Canvas) -> i32 {
    terminal::big_rows(shadow_face(c))
}

fn short_duration(sec: Option<f64>) -> String {
    sec.map(terminal::short_duration)
        .unwrap_or_else(|| "--".into())
}
fn doing(a: &Agent, pc: Option<&Pace>, s: &State, w: i32, p: &Palette) -> Option<(String, u32)> {
    if model::needs_you(a) && a.open_asks.total() > 0 && !model::shows_blocking_now(a) {
        return None;
    }
    if model::shows_blocking_now(a) && a.now.as_ref().is_none_or(|i| i.kind != "user_input_request") {
        return Some((fit("◆ user input request", w), p.dim));
    }
    let (lead, label, tail, color) = if let Some(pc) = pc.filter(|p| p.quiet) {
        let last = a
            .items
            .iter()
            .find(|i| model::shows_recent_activity(a, i));
        (
            format!(
                "◌ {} {}",
                if pc.stalled { "stalled" } else { "quiet" },
                duration(Some(pc.quiet_ms / 1000.))
            ),
            last.map(|i| {
                format!(
                    " · last {} {}",
                    glyph(model::category(&i.kind, i.tool.as_deref())),
                    i.display(&s.home)
                )
            })
            .unwrap_or_default(),
            String::new(),
            if pc.stalled { p.warn } else { p.text2 },
        )
    } else {
        let item = a.now.as_ref()?;
        let tool = !matches!(item.kind.as_str(), "reasoning" | "assistant_message");
        let tail = pc
            .filter(|pc| tool && pc.tool_ms >= 30000.)
            .map(|pc| {
                format!(
                    " · {}{}",
                    if pc.stalled { "△ " } else { "" },
                    duration(Some(pc.tool_ms / 1000.))
                )
            })
            .unwrap_or_default();
        (
            format!(
                "{} ",
                glyph(model::category(&item.kind, item.tool.as_deref()))
            ),
            item.display(&s.home),
            tail,
            if pc.is_some_and(|pc| pc.stalled) {
                p.warn
            } else {
                p.dim
            },
        )
    };
    let whole = format!("{lead}{label}{tail}");
    let text = if width(&whole) <= w {
        whole
    } else {
        let room = w - width(&lead) - width(&tail);
        if room >= 4 {
            format!("{lead}{}{tail}", fit(&label, room))
        } else {
            fit(&format!("{lead}{tail}"), w)
        }
    };
    let _ = s;
    Some((text, color))
}

fn card_time(a: &Agent, kind: &str) -> Option<f64> {
    let item = a
        .items
        .iter()
        .find(|i| i.kind == kind && i.status == "waiting")?;
    model::ms(item.started_at.as_deref()).or_else(|| model::ms(Some(&item.updated_at)))
}

fn draw_asks(c: &mut Canvas, r: Rect, mut y: i32, a: &Agent, s: &State, p: &Palette) -> i32 {
    let asks = &a.open_asks;
    let age = |at: f64| terminal::short_duration((s.now - at).max(0.) / 1000.);
    let question_age = |n| {
        if asks.blocking + asks.nonblocking != n || model::pending_count(a) > asks.total() {
            return String::new();
        }
        model::ms(a.asked.as_deref())
            .map(|at| format!(" · {}", age(at)))
            .unwrap_or_default()
    };
    let keeps_going = if model::running(a) {
        " · the run keeps going"
    } else {
        ""
    };
    let mut line = |glyph, text: String, tail: String, estimated| {
        c.text(r.x + 2, y, "ask", p.dim);
        let tone = if estimated { p.text2 } else { p.hot };
        let mut x = r.x + 7 + c.text_with(r.x + 7, y, glyph, tone, None, glyph == "◆ ", usize::MAX);
        let room = r.right() - x;
        x += c.text(
            x,
            y,
            &fit(&text, room),
            if estimated { p.text2 } else { p.fg },
        );
        if width(&text) + width(&tail) <= room {
            c.text(x, y, &tail, p.dim);
        }
        y += 1;
    };
    if asks.blocking > 0 && !model::shows_blocking_now(a) {
        let n = asks.blocking;
        line(
            "◆ ",
            format!("{n} blocking question{}", crate::ui::plural(n)),
            question_age(n),
            false,
        );
    }
    if asks.cards.secret > 0 {
        let n = asks.cards.secret;
        let text = if n == 1 {
            "a secret to enter in T3".into()
        } else {
            format!("{n} secrets to enter in T3")
        };
        let tail = card_time(a, "secret_request")
            .filter(|_| n == 1)
            .map(|at| format!(" · asked {} ago", age(at)))
            .unwrap_or_default();
        line("◆ ", text, tail, false);
    }
    if asks.cards.link > 0 {
        let n = asks.cards.link;
        let text = if n == 1 {
            "a link to open in T3".into()
        } else {
            format!("{n} links to open in T3")
        };
        let tail = if model::running(a) {
            keeps_going.into()
        } else if n == 1 && !model::live(a) && card_time(a, "link_request").is_some() {
            model::ms(a.ended.as_deref())
                .map(|at| format!(" · waiting {}, since the run ended", age(at)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mark = if model::running(a) { "◇ " } else { "◆ " };
        line(mark, text, tail, false);
    }
    if asks.nonblocking > 0 {
        let n = asks.nonblocking;
        line(
            "◇ ",
            format!("{n} question{} open", crate::ui::plural(n)),
            question_age(n) + keeps_going,
            false,
        );
    }
    if a.estimated_asks > 0 {
        let n = a.estimated_asks;
        let plural = crate::ui::plural(n);
        let text = format!("{n} ask{plural} in an agent reply");
        let tail = " · estimated from the text";
        if 8 + width(&text) + width(tail) <= r.w {
            line("~", text, tail.into(), true);
        } else {
            line(
                "~",
                format!("{n} estimated ask{plural} in agent prose"),
                String::new(),
                true,
            );
        }
    }
    y
}

fn title(c: &mut Canvas, r: Rect, a: &Agent, s: &State, p: &Palette, pc: Option<&Pace>) -> i32 {
    let mut y = r.y;
    let (mark, status, tone) = status(a, s.now, pc, s.calm, s.acked.contains(&a.id), p);
    c.text_bold(r.x, y, &fit(&a.title, r.w), p.bright);
    y += 1;
    let mut meta = vec![a.model.clone(), a.project.clone(), a.kind.clone()];
    for opt in &a.options {
        if let (Some(id), Some(v)) = (opt.get("id").and_then(Value::as_str), opt.get("value")) {
            meta.push(format!(
                "{id} {}",
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            ));
        }
    }
    if let Some(branch) = &a.branch {
        meta.push(format!("⌥ {branch}"));
    }
    c.set(r.x, y, '●', p.provider(a), None, false);
    c.text(r.x + 2, y, &fit(&meta.join(" · "), r.w - 2), p.dim);
    y += 1;
    let mut x = r.x + c.text_bold(r.x, y, &format!("{mark} {status}"), tone);
    // A single blocking runtime question already has a reference presentation.
    // Its status clock measures the run. The attention tile reports the wait.
    let wait_start = if a.open_asks.blocking > 0
        && a.open_asks.nonblocking == 0
        && !model::shows_blocking_now(a)
    {
        model::ms(a.asked.as_deref())
    } else if a.open_asks.cards.secret == 1 {
        card_time(a, "secret_request")
    } else if a.open_asks.cards.link == 1
        && !model::live(a)
        && card_time(a, "link_request").is_some()
    {
        model::ms(a.ended.as_deref())
    } else {
        None
    };
    let start = wait_start.or_else(|| model::ms(a.started.as_deref()));
    let end = model::ms(a.ended.as_deref());
    let counting = model::live(a) || model::needs_you(a);
    let stop = if counting { Some(s.now) } else { end };
    let asked = if !a.requests.is_empty() || a.open_asks.blocking > 0 {
        let mut kinds = Vec::new();
        for kind in &a.requests {
            if a.open_asks.blocking == 0 || kind != "user_input" || !kinds.contains(&"user_input") {
                kinds.push(kind.as_str());
            }
        }
        if a.open_asks.blocking > 0 && !kinds.contains(&"user_input") {
            kinds.push("user_input");
        }
        format!("  · {}", kinds.join(", ").replace('_', " "))
    } else if a.open_asks.cards.secret > 0 {
        "  · secret".into()
    } else if !model::live(a) && a.open_asks.cards.link > 0 {
        "  · link".into()
    } else {
        String::new()
    };
    let mut parts = Vec::new();
    if let Some(start) = start {
        if let Some(stop) = stop {
            parts.push((
                format!("  {}", duration(Some((stop - start) / 1000.))),
                p.text2,
            ));
        }
        parts.push((
            format!(
                "  {}",
                if !counting && end.is_none() {
                    format!("started {}", crate::terminal::clock(start, s.now))
                } else {
                    crate::terminal::when(start, if counting { None } else { end }, s.now)
                }
            ),
            p.text2,
        ));
        if !counting {
            if let Some(end) = end {
                let mut ago = duration(Some((s.now - end) / 1000.));
                if ago.ends_with('s') {
                    if let Some(i) = ago.rfind(' ') {
                        ago.truncate(i);
                    }
                }
                parts.push((format!(" · {ago} ago"), p.dim));
            }
        }
    }
    while parts.len() > 1
        && width(&(parts.iter().map(|(t, _)| t.as_str()).collect::<String>() + &asked))
            > r.x + r.w - x
    {
        parts.pop();
    }
    for (text, color) in parts {
        x += c.text(x, y, &fit(&text, r.x + r.w - x), color);
    }
    if !asked.is_empty() {
        c.text(x, y, &fit(&asked, r.x + r.w - x), p.hot);
    }
    y += 1;
    y = draw_asks(c, r, y, a, s, p);
    if model::live(a) || model::needs_you(a) {
        if let Some((body, tone)) = doing(a, pc, s, r.w - 7, p) {
            c.text(
                r.x,
                y,
                &format!("  now  {body}"),
                if tone == p.dim { p.text2 } else { tone },
            );
            y += 1;
            if pc.is_some_and(|pc| pc.stalled) {
                let why = if pc.is_some_and(|pc| pc.quiet) {
                    "  no activity for a while: check it in T3 (⏎)"
                } else {
                    "  one call running over 5m"
                };
                c.text(r.x, y, &fit(why, r.w), p.dim);
                y += 1;
            }
        }
    }
    if a.reference_only == Some(true) {
        c.text(
            r.x,
            y,
            &fit(
                "  Parent reference · state unavailable in this local snapshot",
                r.w,
            ),
            p.accent,
        );
        y += 1;
    } else if let Some(remote) = &a.remote {
        c.text(
            r.x,
            y,
            &fit(
                &format!(
                    "  remote on {}; context and activity live there",
                    remote
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("another environment")
                ),
                r.w,
            ),
            p.accent,
        );
        y += 1;
    }
    y - r.y + 1
}

#[derive(Clone, Copy)]
enum Block {
    Failures,
    Context,
    Growth,
    Tokens,
    Cache,
    Plan,
    Tools,
    Activity,
}
fn block(
    c: &mut Canvas,
    kind: Block,
    x: i32,
    y: i32,
    w: i32,
    room: i32,
    r: Rect,
    a: &Agent,
    s: &State,
    p: &Palette,
    pc: Option<&Pace>,
) -> i32 {
    match kind {
        Block::Failures => {
            let minutes = model::WINDOWS[s.window_index % 4];
            let failed: Vec<_> = a
                .recent_failures
                .iter()
                .filter(|i| {
                    model::ms(Some(&i.updated_at)).unwrap_or(0.) >= s.now - minutes as f64 * 60000.
                })
                .collect();
            if failed.is_empty() {
                return 0;
            }
            let mut yy = y;
            section(
                c,
                x,
                yy,
                w,
                "RECENT FAILURES",
                &format!("{} · {minutes}m", failed.len()),
                p,
                p.hot,
            );
            yy += 1;
            for i in failed {
                let age = short_duration(age(Some(&i.updated_at), s.now));
                let exit = i
                    .exit_code
                    .map(|n| format!("exit {n}"))
                    .unwrap_or_else(|| "failed".into());
                let old = if i
                    .run_id
                    .as_deref()
                    .is_some_and(|id| Some(id) != a.run.get("id").and_then(Value::as_str))
                {
                    " · earlier run"
                } else {
                    ""
                };
                let meta = format!("{age} ago · {exit}{old}");
                let lines = wrap(&format!("{meta}  {}", i.display(&s.home)), w - 2);
                c.set(x, yy, '✗', p.hot, None, false);
                let top = yy;
                for line in lines {
                    c.text(x + 2, yy, &line, p.text2);
                    yy += 1
                }
                c.text(x + 2, top, &fit(&meta, w - 2), p.hot);
            }
            c.text(
                x,
                yy,
                &fit("Failed calls; agent status is separate. Enter opens T3.", w),
                p.dim,
            );
            yy + 2 - y
        }
        Block::Context => {
            let reported = a
                .context
                .updated_at
                .as_deref()
                .map(|t| format!("reported {} ago", duration(age(Some(t), s.now))))
                .unwrap_or_default();
            section(c, x, y, w, "CONTEXT", &reported, p, p.dim);
            let mut yy = y + 1;
            let Some(rr) = ratio(a) else {
                c.text(
                    x,
                    yy,
                    &fit("Context usage not reported for this agent.", w),
                    p.dim,
                );
                return yy + 2 - y;
            };
            let color = if rr >= 0.9 {
                p.hot
            } else if rr >= 0.75 {
                p.warn
            } else {
                p.bright
            };
            let fw = big(c, x, yy, &percent(rr * 100.), color, p);
            let tx = x + fw + 3;
            c.text_bold(
                tx,
                yy,
                &fit(
                    &format!(
                        "{} of {} tokens",
                        tokens(a.context.used_tokens),
                        tokens(a.context.max_tokens)
                    ),
                    x + w - tx,
                ),
                p.fg,
            );
            if a.history.len() >= 2 {
                let prev = &a.history[a.history.len() - 2];
                let last = a.history.last().unwrap();
                if prev.provider == last.provider {
                    let delta = last.used - prev.used;
                    c.text(
                        tx,
                        yy + 1,
                        &fit(
                            &format!(
                                "{}{} last turn",
                                if delta >= 0. { "+" } else { "−" },
                                tokens(Some(delta.abs()))
                            ),
                            x + w - tx,
                        ),
                        p.text2,
                    );
                }
            }
            c.text(
                tx,
                yy + 2,
                &fit(
                    &format!(
                        "{} free",
                        tokens(Some(
                            a.context.max_tokens.unwrap_or(0.)
                                - a.context.used_tokens.unwrap_or(0.)
                        ))
                    ),
                    x + w - tx,
                ),
                p.dim,
            );
            yy += big_rows(c);
            let shown = s.tween.get(&a.id).copied().unwrap_or(rr);
            context_bar(c, x, yy, w, shown, p);
            yy += 1;
            for (t, label, tone) in [(0.5, "50", p.dim), (0.75, "75", p.warn), (0.9, "90", p.hot)] {
                c.text(
                    x + (t * w as f64).round() as i32 - 1,
                    yy,
                    &format!("╵{label}"),
                    tone,
                );
            }
            yy + 2 - y
        }
        Block::Growth => {
            if a.history.len() < 2 {
                return 0;
            }
            let h = 3.max(8.min(r.h / 7).min(room - 3));
            section(
                c,
                x,
                y,
                w,
                "GROWTH",
                &format!("{} turns · % of window", a.history.len()),
                p,
                p.dim,
            );
            growth(c, x, y + 1, w, h, a, p);
            h + 3
        }
        Block::Tokens => {
            let usage = a.turn.get("turnTokenUsage").filter(|v| v.is_object());
            let call = a.turn.get("tokenUsage").filter(|v| v.is_object());
            let Some(src) = usage.or(call) else { return 0 };
            let cached = number(src, "cachedInputTokens");
            let fresh = (number(src, "inputTokens") - cached).max(0.);
            let output = number(src, "outputTokens");
            let reasoning = src
                .get("reasoningTokens")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| number(src, "reasoningOutputTokens"));
            let parts: Vec<_> = [
                ("cached", cached, mix(p.accent, p.bg, 0.45)),
                ("fresh", fresh, p.accent),
                ("output", output, p.bright),
                ("reasoning", reasoning, p.warn),
            ]
            .into_iter()
            .filter(|p| p.1 > 0.)
            .collect();
            section(
                c,
                x,
                y,
                w,
                if usage.is_some() {
                    "LAST TURN"
                } else {
                    "LATEST CALL"
                },
                if usage.is_some() {
                    src.get("usageStatus").and_then(Value::as_str).unwrap_or("")
                } else {
                    "turn total pending"
                },
                p,
                p.dim,
            );
            segmented(c, x, y + 1, w, &parts, p);
            let mut lx = x;
            for (label, n, color) in parts {
                let label = format!("{label} {}", tokens(Some(n)));
                if lx + width(&label) + 2 > x + w {
                    break;
                }
                c.set(lx, y + 2, '■', color, None, false);
                lx += 2 + c.text(lx + 2, y + 2, &label, p.text2) + 2;
            }
            if usage.is_some_and(|v| v.get("hasSubagents").and_then(Value::as_bool) == Some(true)) {
                c.text(
                    x,
                    y + 3,
                    &fit("includes subagent usage; not added again elsewhere", w),
                    p.dim,
                );
                5
            } else {
                4
            }
        }
        Block::Cache => {
            let call = a.turn.get("tokenUsage").unwrap_or(&Value::Null);
            let exp = model::ms(
                a.context
                    .prompt_cache_expires_at
                    .as_deref()
                    .or_else(|| call.get("promptCacheExpiresAt").and_then(Value::as_str)),
            );
            let ttl = a
                .context
                .prompt_cache_ttl_ms
                .or_else(|| call.get("promptCacheTtlMs").and_then(Value::as_f64));
            let (Some(exp), Some(ttl)) = (exp, ttl) else {
                return 0;
            };
            if ttl <= 0. {
                return 0;
            }
            let left = exp - s.now;
            let frac = (left / ttl).clamp(0., 1.);
            let label = if left > 0. {
                format!("warm · {} left", short_duration(Some(left / 1000.)))
            } else {
                format!("cold · {} ago", short_duration(Some(-left / 1000.)))
            };
            c.text_bold(x, y, "PROMPT CACHE", p.text2);
            hbar(
                c,
                x + 14,
                y,
                (w - 14 - width(&label) - 2).max(4),
                frac,
                p.track,
                |_, _| {
                    if frac < 0.12 {
                        p.warn
                    } else {
                        mix(p.accent, p.bright, frac * 0.6)
                    }
                },
            );
            c.right(x + w, y, &label, if left > 0. { p.text2 } else { p.dim });
            2
        }
        Block::Plan => {
            let Some(plan) = a.plan.as_ref().filter(|p| !p.is_empty()) else {
                return 0;
            };
            let done = plan.iter().filter(|q| q.status == "completed").count();
            section(
                c,
                x,
                y,
                w,
                "PLAN",
                &format!("{done}/{} done", plan.len()),
                p,
                p.dim,
            );
            let each = w as f64 / plan.len() as f64;
            for (i, q) in plan.iter().enumerate() {
                let tone = if q.status == "completed" {
                    p.accent
                } else if q.status == "in_progress" {
                    p.bright
                } else {
                    p.track
                };
                let begin = (i as f64 * each).round() as i32;
                let end =
                    (((i + 1) as f64 * each).round() as i32 - i32::from(each >= 3.)).max(begin + 1);
                for k in begin..end {
                    c.set(x + k, y + 1, '▀', tone, None, false)
                }
            }
            let mut yy = y + 2;
            for q in plan.iter().take(8) {
                let (g, tone) = if q.status == "completed" {
                    ('✓', p.dim)
                } else if q.status == "in_progress" {
                    ('▸', p.bright)
                } else {
                    ('○', p.text2)
                };
                c.set(x, yy, g, tone, None, false);
                c.text(x + 2, yy, &fit(&q.text, w - 2), tone);
                yy += 1
            }
            if plan.len() > 8 {
                c.text(x + 2, yy, &format!("+{} more", plan.len() - 8), p.dim);
                yy += 1
            }
            yy + 1 - y
        }
        Block::Tools => {
            let mut entries: Vec<_> = a.mix.iter().collect();
            // Stable count sorting retains the original JSON insertion order for ties.
            entries.sort_by(|a, b| b.1.cmp(a.1));
            if entries.is_empty() {
                return 0;
            }
            let total: u64 = entries.iter().map(|(_, n)| **n).sum();
            let mut right = format!(
                "{total} {}",
                if a.run.get("id").and_then(Value::as_str).is_some() {
                    "this run"
                } else {
                    "in thread"
                }
            );
            if a.failures > 0 {
                right += &format!(" · {} failed", a.failures)
            }
            section(
                c,
                x,
                y,
                w,
                "TOOLS",
                &right,
                p,
                if a.failures > 0 { p.hot } else { p.dim },
            );
            let max = *entries[0].1;
            let nw = entries
                .iter()
                .map(|(_, n)| n.to_string().len())
                .max()
                .unwrap_or(1) as i32;
            let mut yy = y + 1;
            for (cat, n) in entries.iter().take(7) {
                c.set(x, yy, glyph(cat), p.text2, None, false);
                c.text(x + 2, yy, cat, p.text2);
                hbar(
                    c,
                    x + 9,
                    yy,
                    (w - 10 - nw).max(4),
                    **n as f64 / max.max(1) as f64,
                    p.bg,
                    |i, n| ramp(p, terminal::along(0.35, 0.55, i as f64, n as f64)),
                );
                c.right(x + w, yy, &n.to_string(), p.fg);
                yy += 1
            }
            yy + 1 - y
        }
        Block::Activity => {
            let feed: Vec<_> = a
                .items
                .iter()
                .filter(|i| !matches!(i.kind.as_str(), "reasoning" | "assistant_message"))
                .collect();
            let mut yy = y;
            section(
                c,
                x,
                yy,
                w,
                "ACTIVITY",
                if a.run.get("id").and_then(Value::as_str).is_some() {
                    "latest run"
                } else {
                    "thread activity"
                },
                p,
                p.dim,
            );
            yy += 1;
            if feed.is_empty() {
                c.text(
                    x,
                    yy,
                    &fit(
                        if a.run.get("id").and_then(Value::as_str).is_some() {
                            "No tool activity recorded for this run."
                        } else {
                            "No tool activity recorded for this thread."
                        },
                        w,
                    ),
                    p.dim,
                );
                return yy + 2 - y;
            }
            let limit = (room - 2).clamp(6, 16) as usize;
            let moving = matches!(a.status.as_str(), "running" | "active")
                && !model::needs_you(a)
                && !pc.is_some_and(|p| p.quiet);
            for item in feed.iter().take(limit) {
                let seen = s
                    .first_seen
                    .get(&model::item_key(item))
                    .copied()
                    .unwrap_or(0.);
                let fresh = (s.now - seen).max(0.);
                let flash = if seen > 0. && fresh < 1600. {
                    if s.calm { 1. } else { 1. - fresh / 1600. }
                } else {
                    0.
                };
                if flash > 0. {
                    c.fill(x, yy, w, 1, mix(p.bg, p.accent, 0.35 * flash));
                }
                let running = live_status(&item.status);
                let failed = item.status == "failed";
                let age = age(
                    if running {
                        item.started_at.as_deref().or(Some(&item.updated_at))
                    } else {
                        Some(&item.updated_at)
                    },
                    s.now,
                )
                .unwrap_or(0.);
                let cat = model::category(&item.kind, item.tool.as_deref());
                let fade = if running {
                    p.bright
                } else if age < 15. {
                    p.fg
                } else if age < 90. {
                    p.text2
                } else {
                    p.dim
                };
                let asking = running && cat == "ask";
                let mark = if asking {
                    '◆'
                } else if running {
                    if moving {
                        spinner(s.now, s.calm)
                    } else {
                        glyph(cat)
                    }
                } else if failed {
                    '✗'
                } else {
                    glyph(cat)
                };
                c.set(
                    x,
                    yy,
                    mark,
                    if asking || failed {
                        p.hot
                    } else if running {
                        p.bright
                    } else {
                        mix(fade, p.accent, 0.3)
                    },
                    None,
                    asking,
                );
                c.text(
                    x + 2,
                    yy,
                    &format!("{:>6}", short_duration(Some(age))),
                    if running { p.text2 } else { p.dim },
                );
                c.text(
                    x + 9,
                    yy,
                    &fit(&item.display(&s.home), w - 9),
                    if failed { mix(p.hot, fade, 0.3) } else { fade },
                );
                yy += 1;
            }
            if feed.len() > limit {
                c.text(
                    x,
                    yy,
                    &fit(
                        &format!("▼ {} more calls · Enter opens T3", feed.len() - limit),
                        w,
                    ),
                    p.dim,
                );
                yy += 1
            }
            yy + 1 - y
        }
    }
}

fn segmented(c: &mut Canvas, x: i32, y: i32, w: i32, parts: &[(&str, f64, u32)], p: &Palette) {
    if w <= 0 {
        return;
    }
    let total: f64 = parts.iter().map(|p| p.1).sum();
    if total <= 0. {
        c.fill(x, y, w, 1, p.track);
        return;
    }
    let mut cells: Vec<i32> = parts
        .iter()
        .map(|p| ((p.1 / total * w as f64).round() as i32).max(1))
        .collect();
    let mut over = cells.iter().sum::<i32>() - w;
    while over > 0 {
        let Some((i, _)) = cells
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(&a.0)))
        else {
            break;
        };
        cells[i] -= 1;
        over -= 1;
    }
    let mut xx = x;
    for ((_, _, color), n) in parts.iter().zip(cells) {
        for _ in 0..n {
            c.set(xx, y, '█', *color, None, false);
            xx += 1
        }
    }
    for xx in xx..x + w {
        c.set(xx, y, ' ', p.track, Some(p.track), false)
    }
}

fn growth(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32, a: &Agent, p: &Palette) {
    let axis = 5;
    let cols = (w - axis).max(4);
    let pw = cols * 2;
    let ph = h * 4;
    let points = &a.history[a.history.len().saturating_sub(pw as usize)..];
    let n = points.len();
    if n == 0 || h <= 0 {
        return;
    }
    let px = |i: usize| {
        if n == 1 {
            0
        } else {
            (i as f64 * (pw - 1) as f64 / (n - 1) as f64).round() as i32
        }
    };
    let mut bits = vec![0u8; (cols * h) as usize];
    let maps = [[1u8, 8u8], [2, 16], [4, 32], [64, 128]];
    let mut column = |xx: i32, frac: f64| {
        let top = ph - (frac.clamp(0., 1.) * ph as f64).round() as i32;
        for yy in top..ph {
            if xx >= 0 && xx < pw && yy >= 0 {
                bits[((yy / 4) * cols + xx / 2) as usize] |=
                    maps[(yy % 4) as usize][(xx % 2) as usize]
            }
        }
    };
    for i in 0..n.saturating_sub(1) {
        let (x0, x1) = (px(i), px(i + 1));
        for xx in x0..=x1 {
            let t = if x1 == x0 {
                0.
            } else {
                (xx - x0) as f64 / (x1 - x0) as f64
            };
            column(xx, points[i].pct + (points[i + 1].pct - points[i].pct) * t);
        }
    }
    if n == 1 {
        column(0, points[0].pct);
    }
    let mut memo = crate::terminal::FamilyMemo::default();
    let colors: Vec<_> = (0..cols)
        .map(|col| {
            let point =
                ((col * 2) as f64 / (pw - 1).max(1) as f64 * (n - 1) as f64).round() as usize;
            let point = &points[point.min(n - 1)];
            memo.family(&point.model, Some(&point.provider))
        })
        .collect();
    for row in 0..h {
        for col in 0..cols {
            let b = bits[(row * cols + col) as usize];
            if b == 0 {
                continue;
            }
            let top = 1. - row as f64 / h as f64;
            let color = if top > 0.9 {
                p.hot
            } else if top > 0.75 {
                p.warn
            } else {
                if colors[col as usize] == crate::terminal::ProviderFamily::Other {
                    ramp(p, 0.3 + 0.7 * top)
                } else {
                    mix(
                        p.family_color(colors[col as usize]),
                        p.bright,
                        0.1 + 0.35 * top,
                    )
                }
            };
            c.set(
                x + axis + col,
                y + row,
                char::from_u32(0x2800 + b as u32).unwrap(),
                color,
                None,
                false,
            );
        }
    }
    for i in 1..n {
        if points[i].provider == points[i - 1].provider {
            continue;
        }
        let cx = x + axis + px(i) / 2;
        for yy in y..y + h {
            c.set(cx, yy, '┊', p.faint, None, false)
        }
        let tag = fit(
            &format!(
                "→ {}",
                short_model(if points[i].model.is_empty() {
                    "handoff"
                } else {
                    &points[i].model
                })
            ),
            16,
        );
        c.text(
            if cx + 1 + width(&tag) <= x + w {
                cx + 1
            } else {
                cx - width(&tag)
            },
            y,
            &tag,
            p.dim,
        );
    }
    c.right(x + axis - 1, y, "100", p.dim);
    c.right(x + axis - 1, y + h - 1, "0", p.dim);
    if h >= 4 {
        c.right(x + axis - 1, y + h / 2, "50", p.dim);
    }
    c.text(x + axis, y + h, "first turn", p.dim);
    c.right(x + w, y + h, "latest", p.dim);
}

fn ago_span(agents: &[&Agent], now: f64) -> String {
    let seen: Vec<_> = agents
        .iter()
        .map(|a| model::last_seen(a))
        .filter(|t| *t > 0.)
        .collect();
    if seen.is_empty() {
        return String::new();
    }
    let newest = seen.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let oldest = seen.iter().copied().fold(f64::INFINITY, f64::min);
    let a = coarse(Some((now - newest) / 1000.));
    let b = coarse(Some((now - oldest) / 1000.));
    if a == b {
        format!("{a} ago")
    } else {
        format!("{a}–{b} ago")
    }
}
fn earlier(
    c: &mut Canvas,
    r: Rect,
    s: &State,
    indices: &[usize],
    parent: &Agent,
    p: &Palette,
) -> usize {
    let list: Vec<&Agent> = indices.iter().map(|i| &*s.snap.agents[*i]).collect();
    let span = ago_span(&list, s.now);
    let mut y = r.y;
    c.text_bold(r.x, y, &fit(&parent.title, r.w), p.bright);
    y += 1;
    c.text(
        r.x,
        y,
        &fit(
            &format!(
                "{} earlier subagent{}{}",
                list.len(),
                if list.len() == 1 { "" } else { "s" },
                if span.is_empty() {
                    String::new()
                } else {
                    format!(" · last active {span}")
                }
            ),
            r.w,
        ),
        p.text2,
    );
    y += 1;
    c.text_bold(r.x, y, "⏎ → or a click", p.accent);
    c.text(r.x + 15, y, &fit("lists them all", r.w - 15), p.text2);
    y += 2;
    let failed = list
        .iter()
        .filter(|a| matches!(a.status.as_str(), "failed" | "error"))
        .count();
    section(
        c,
        r.x,
        y,
        r.w,
        "EARLIER",
        &format!(
            "newest first{}",
            if failed > 0 {
                format!(" · {failed} ✗")
            } else {
                String::new()
            }
        ),
        p,
        if failed > 0 { p.hot } else { p.dim },
    );
    y += 1;
    for a in list {
        let (g, _, tone) = status(a, s.now, None, s.calm, false, p);
        let meta = short_model(&a.model);
        let mw = width(&meta).min((r.w - 24).max(0));
        c.set(r.x, y, g, tone, None, true);
        let seen = model::last_seen(a);
        let age = if seen > 0. {
            coarse(Some((s.now - seen).max(0.) / 1000.))
        } else {
            "--".into()
        };
        c.text(r.x + 2, y, &format!("{age:>4}"), p.dim);
        c.text(
            r.x + 7,
            y,
            &fit(&a.title, (r.w - 7 - if mw > 0 { mw + 4 } else { 0 }).max(4)),
            p.text2,
        );
        if mw > 0 {
            c.right_max(r.x + r.w, y, &meta, p.dim, None, false, mw as usize);
            c.set(r.x + r.w - mw - 2, y, '●', p.provider(a), None, false)
        }
        y += 1;
    }
    y += 1;
    let why = "Folded away: last active over 3h ago, or before the 3 latest. Running subagents and any waiting on you always stay listed.";
    for line in wrap(why, r.w) {
        c.text(r.x, y, &line, p.dim);
        y += 1;
    }
    (y - r.y).max(0) as usize
}

/// Draws the selected agent/earlier row at the same logical cell coordinates as OpenTUI.
/// The returned logical height drives the caller's detail scroll boundary and arrows.
pub(crate) fn draw_details(
    c: &mut Canvas,
    r: Rect,
    s: &State,
    p: &Palette,
    prepared: &Prepared,
    scroll: usize,
) -> usize {
    if r.w <= 0 || r.h <= 0 {
        return 0;
    }
    let Some(row) = s
        .selected
        .as_ref()
        .and_then(|id| prepared.row_by_id.get(id))
        .map(|i| &prepared.rows[*i])
    else {
        return 0;
    };
    let a = &s.snap.agents[row.agent];
    let current_pace = prepared.pace(row.agent, s);
    let pc = current_pace.as_ref();
    let mut height = 0;
    c.clipped(r.x, r.y, r.w, r.h, scroll as i32, |c| {
        if row.is_earlier() {
            height = earlier(c, r, s, &row.earlier, a, p);
            return;
        }
        let top = r.y + title(c, r, a, s, p, pc);
        if r.w >= 100 {
            let cw = (r.w - 3) / 2;
            let mut ly = top;
            for kind in [Block::Context, Block::Growth, Block::Tokens, Block::Cache] {
                ly += block(c, kind, r.x, ly, cw, r.y + r.h - ly, r, a, s, p, pc)
            }
            let mut ry = top;
            for kind in [Block::Failures, Block::Plan, Block::Activity, Block::Tools] {
                ry += block(
                    c,
                    kind,
                    r.x + cw + 3,
                    ry,
                    r.w - cw - 3,
                    r.y + r.h - ry,
                    r,
                    a,
                    s,
                    p,
                    pc,
                )
            }
            for yy in top..ly.max(ry) - 1 {
                c.set(r.x + cw + 1, yy, '┊', p.track, None, false)
            }
            height = (ly.max(ry) - r.y).max(0) as usize;
        } else {
            let mut y = top;
            for kind in [
                Block::Failures,
                Block::Context,
                Block::Plan,
                Block::Activity,
                Block::Tools,
                Block::Growth,
                Block::Tokens,
                Block::Cache,
            ] {
                y += block(c, kind, r.x, y, r.w, r.y + r.h - y, r, a, s, p, pc)
            }
            height = (y - r.y).max(0) as usize;
        }
    });
    height
}

pub fn draw_help(c: &mut Canvas, s: &State, p: &Palette) -> usize {
    let all_keys = [
        ("↑↓ j k", "select agent"),
        ("n", "next agent waiting on you"),
        ("i", "insights: Recap and Tools"),
        ("⏎ o", "open the thread in T3 Code"),
        ("␣ ← →", "fold · unfold · earlier"),
        ("a", "all · active · T3 sidebar"),
        ("t", "lane 15m · 30m · 1h · 3h"),
        ("J K PgDn", "scroll details"),
        ("f", "follow the thread open in T3"),
        ("p", "pause updates"),
        ("x", "CRT effects"),
        ("r", "refresh now"),
        ("m", "calm mode: nothing moves"),
        ("mouse", "click selects, wheel scrolls"),
        ("c C", "theme: next · back"),
        ("w", "trace: the agent's task on a clock"),
        ("s", "storage: disk use by project and thread"),
        ("q Esc", "quit (Esc closes help)"),
    ];
    // `c` is the product's; the original's help, which the parity checks compare, has no theme key.
    let keys: Vec<_> = all_keys
        .into_iter()
        .filter(|k| match k.0 {
            "c C" | "w" => s.themes,
            "s" | "i" => s.storage_key_in_help,
            _ => true,
        })
        .collect();
    let mut legend = vec![
        (
            "⠹ ◆ ✓ ✗ ⊘",
            "running · needs you (blinks until seen) · done · failed · interrupted",
        ),
        ("█████▌", "context used; warm past 75%, hot past 90%"),
        ("▂▄█", "lane: tool and model activity per time slice"),
        ("tint", "lane: a run was in progress"),
        ("┃  ×", "lane: a prompt you sent · a failed call"),
        (
            "14:02╷",
            "lane axis: when the selected run started and ended",
        ),
        (
            "27m",
            "how long the latest run took; brighter while it counts up",
        ),
        (
            "│",
            "limit meter: time elapsed; fill beyond it is ahead of pace",
        ),
        ("▎ band", "the thread open in T3 Code and its subagents"),
        (
            "┄ earlier",
            "subagents done over 3h ago or beyond the 3 latest; ⏎ lists them",
        ),
        ("◌", "running, but nothing new for 2m; \"stalled\" after 8m"),
        ("◔  ◉", "queued, not started yet · running, in calm mode"),
        ("❯ · △ 6m", "a tool call's run time; △ slow past 5m"),
    ];
    if s.themes || s.snap.agents.iter().any(|a| {
        a.estimated_asks > 0 || (a.open_asks.total() > 0 && !model::shows_blocking_now(a))
    }) {
        legend.extend([
            ("◆2", "pending formal asks on this thread"),
            ("◇2", "questions open without blocking the run"),
            ("cards", "secret and link cards waiting for you in T3 Code"),
            ("~N", "estimated asks in agent replies"),
        ]);
    }
    let w = c.w as i32;
    let h = c.h as i32;
    if w < 6 || h < 6 {
        return 0;
    }
    let two = w >= 84;
    let bw = (w - 2).min(if two { 80 } else { 64 });
    let key_rows = if two {
        keys.len().div_ceil(2)
    } else {
        keys.len()
    } as i32;
    let full = key_rows + legend.len() as i32 + 10;
    let bh = (h - 2).min(full);
    let max = (full - bh).max(0);
    let scroll = (s.help_scroll as i32).min(max);
    let bx = (w - bw) / 2;
    let by = (h - bh) / 2;
    // The baseline dims the dashboard underneath; if the canvas supports tint in
    // the host this panel remains exactly the same cells and dimensions.
    c.fill(bx, by, bw, bh, p.panel);
    for x in bx + 1..bx + bw - 1 {
        c.set(x, by, '─', p.accent, None, false);
        c.set(x, by + bh - 1, '─', p.accent, None, false)
    }
    for y in by + 1..by + bh - 1 {
        c.set(bx, y, '│', p.accent, None, false);
        c.set(bx + bw - 1, y, '│', p.accent, None, false)
    }
    for (x, y, g) in [
        (bx, by, '╭'),
        (bx + bw - 1, by, '╮'),
        (bx, by + bh - 1, '╰'),
        (bx + bw - 1, by + bh - 1, '╯'),
    ] {
        c.set(x, y, g, p.accent, None, false)
    }
    c.text_bold(bx + 2, by, " T3 Agents · help ", p.bright);
    if scroll < max {
        c.right(
            bx + bw - 2,
            by + bh - 1,
            &format!(" ▼ {} more · ↓ ", max - scroll),
            p.bright,
        );
    }
    if scroll > 0 {
        c.right(bx + bw - 2, by, &format!(" ▲ {scroll} more · ↑ "), p.bright);
    }
    c.clipped(bx + 1, by + 1, bw - 2, bh - 2, scroll, |c| {
        let mut y = by + 2;
        let x0 = bx + 3;
        let half = (bw - 6) / 2;
        c.text_bold(x0, y, "KEYS", p.text2);
        y += 1;
        for (i, (key, desc)) in keys.iter().enumerate() {
            let col = if two && i >= key_rows as usize { 1 } else { 0 };
            let yy = y + if two { i as i32 % key_rows } else { i as i32 };
            let x = x0 + col * half;
            c.text_bold(x, yy, key, p.accent);
            c.text(
                x + 11,
                yy,
                &fit(desc, if two { half - 12 } else { bw - 18 }),
                p.fg,
            );
        }
        y += key_rows + 1;
        c.text_bold(x0, y, "LEGEND", p.text2);
        y += 1;
        for (g, desc) in legend {
            c.text(x0, y, g, p.bright);
            c.text(x0 + 11, y, &fit(desc, bw - 17), p.fg);
            y += 1;
        }
        c.text(x0, y, "● provider", p.bright);
        let mut lx = x0 + 11;
        for (name, model) in [
            ("claude", "claude"),
            ("gpt · codex", "gpt"),
            ("grok", "grok"),
            ("other", ""),
        ] {
            let a = Agent {
                model: model.into(),
                ..Agent::default()
            };
            c.set(lx, y, '●', p.provider(&a), None, false);
            lx += 2 + c.text(lx + 2, y, name, p.fg) + 2;
        }
        y += 1;
        c.text(
            x0,
            y + 1,
            &fit(
                "Read-only: local T3 SQLite and Omarchy usage records. No network.",
                bw - 6,
            ),
            p.dim,
        );
        c.text(
            x0,
            y + 2,
            &fit(
                "Unofficial; not affiliated with T3 Code or Omarchy.",
                bw - 6,
            ),
            p.dim,
        );
    });
    max as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    // Insights is a deliberate product-only addition to Live help; the oracle
    // grid has no Insights entry. Check the visible cells, not the key list.
    #[test]
    fn insights_help_is_visible_and_oracle_help_is_unchanged() {
        let p = Palette::fallback();
        for (w, h) in [(55, 24), (80, 24), (120, 40)] {
            let mut s = State::new(model::Snapshot::default());
            s.themes = true;
            s.storage_key_in_help = true;
            s.help = true;
            let (grid, _) = crate::ui::draw_grid(&s, w, h, &p);
            let rows: Vec<_> = (0..h).map(|y| grid.row_text(y)).collect();
            let matches: Vec<_> = rows.iter().enumerate().filter(|(_, row)| row.contains("insights: Recap and Tools")).collect();
            assert_eq!(matches.len(), 1, "{w}x{h}: {rows:?}");
            let (y, row) = matches[0];
            let x = row[..row.find("insights: Recap and Tools").unwrap()].chars().count();
            assert!(y > 0 && y < h - 1 && x >= 11);
            assert_eq!(grid.cells[y * w + x - 11].ch, 'i');
            assert_eq!(grid.cells[y * w + x - 11].fg, p.accent);
            assert_eq!(grid.cells[y * w + x].fg, p.fg);
            assert!(grid.cells[y * w + x - 11].bold);
            // Negative controls: a missing label or a displaced key must fail
            // the same visible-cell contract, even if the key list contains it.
            let visible = |c: &Canvas| {
                (1..h - 1).any(|y| {
                    let row = c.row_text(y);
                    row.find("insights: Recap and Tools")
                        .map(|byte| row[..byte].chars().count())
                        .is_some_and(|x| x >= 11 && c.cells[y * w + x - 11].ch == 'i')
                })
            };
            assert!(visible(&grid));
            let mut missing = grid.clone();
            missing.cells[y * w + x].ch = ' ';
            assert!(!visible(&missing));
            let mut misplaced = grid.clone();
            misplaced.cells[y * w + x - 11].ch = ' ';
            misplaced.cells[y * w + x - 10].ch = 'i';
            assert!(!visible(&misplaced));

            s.themes = false;
            s.storage_key_in_help = false;
            let (oracle, _) = crate::ui::draw_grid(&s, w, h, &p);
            assert!((0..h).all(|y| !oracle.row_text(y).contains("insights")));
        }
    }
    #[test]
    fn original_digits_and_text_fit_remain_cell_based() {
        assert_eq!(width("日本語"), 6);
        assert_eq!(fit("日本語 title", 5), "日本…");
        assert_eq!(terminal::big_glyph('5', true).unwrap()[0], "███████╗");
        assert_eq!(duration(Some(3721.)), "1h 02m");
        assert_eq!(short_model("claude-opus-5-5"), "opus 5.5");
    }
    #[test]
    fn wrapping_keeps_long_paths_bounded() {
        let lines = wrap("first /very/long/path/with/no/spaces last", 8);
        assert!(lines.iter().all(|l| width(l) <= 8));
        assert!(lines.len() > 3);
    }
    /// `statusOf`'s default branch: a status the original does not name shows itself,
    /// uppercased, and keeps the neutral dot. The named ones keep their fixed words.
    #[test]
    fn unseen_statuses_show_themselves_uppercased() {
        let p = Palette::default();
        let mut a = Agent::default();
        for (name, mark, text) in [
            ("unknown", '·', "UNKNOWN"),
            ("settled", '·', "SETTLED"),
            ("snoozed", '·', "SNOOZED"),
            ("cancelled", '·', "CANCELLED"),
            ("", '·', ""),
            ("ßeta", '·', "SSETA"),
            ("ıdentity", '·', "IDENTITY"),
            ("RuNnInG", '·', "RUNNING"),
            ("mixedCase", '·', "MIXEDCASE"),
            ("a-b 1", '·', "A-B 1"),
            ("pending", '◔', "PENDING"),
            ("completed", '✓', "DONE"),
        ] {
            a.status = name.into();
            let (got_mark, got_text, _) = status(&a, 0.0, None, true, false, &p);
            assert_eq!((got_mark, got_text.as_str()), (mark, text), "status {name:?}");
        }
    }
}
