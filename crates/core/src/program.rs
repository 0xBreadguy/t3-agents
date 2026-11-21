//! Listed command names only: no shell execution, expansion or retained input.
//! The SQL scalar consumes borrowed text inside the query and returns only listed names.
//! This intentionally simplified shell grammar is shared by Trace and Tools.

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Tier { Build = 1, Search = 2, Utility = 3 }

/// Every listed program, highest tier first. One list for every feature.
pub const PROGRAMS: &[(&str, Tier)] = &ENTRIES;
// A const initializer alone can be duplicated across call sites. One static backing
// array gives programs(), listed() and callers the same string references.
static ENTRIES: [(&str, Tier); 75] = [
    ("cargo", Tier::Build),
    ("rustc", Tier::Build),
    ("rustup", Tier::Build),
    ("npm", Tier::Build),
    ("npx", Tier::Build),
    ("pnpm", Tier::Build),
    ("yarn", Tier::Build),
    ("bun", Tier::Build),
    ("node", Tier::Build),
    ("deno", Tier::Build),
    ("tsc", Tier::Build),
    ("vitest", Tier::Build),
    ("jest", Tier::Build),
    ("eslint", Tier::Build),
    ("prettier", Tier::Build),
    ("python", Tier::Build),
    ("python3", Tier::Build),
    ("pytest", Tier::Build),
    ("pip", Tier::Build),
    ("uv", Tier::Build),
    ("ruff", Tier::Build),
    ("go", Tier::Build),
    ("make", Tier::Build),
    ("cmake", Tier::Build),
    ("gcc", Tier::Build),
    ("clang", Tier::Build),
    ("docker", Tier::Build),
    ("kubectl", Tier::Build),
    ("java", Tier::Build),
    ("mvn", Tier::Build),
    ("gradle", Tier::Build),
    ("dotnet", Tier::Build),
    ("swift", Tier::Build),
    ("xcodebuild", Tier::Build),
    ("zig", Tier::Build),
    ("ffmpeg", Tier::Build),
    ("ssh", Tier::Build),
    ("rsync", Tier::Build),
    ("curl", Tier::Build),
    ("wget", Tier::Build),
    ("psql", Tier::Build),
    ("sqlite3", Tier::Build),
    ("gh", Tier::Build),
    ("git", Tier::Search),
    ("rg", Tier::Search),
    ("grep", Tier::Search),
    ("fd", Tier::Search),
    ("find", Tier::Search),
    ("jq", Tier::Search),
    ("awk", Tier::Search),
    ("sed", Tier::Search),
    ("diff", Tier::Search),
    ("tar", Tier::Search),
    ("unzip", Tier::Search),
    ("tmux", Tier::Search),
    ("ls", Tier::Utility),
    ("cat", Tier::Utility),
    ("head", Tier::Utility),
    ("tail", Tier::Utility),
    ("wc", Tier::Utility),
    ("sort", Tier::Utility),
    ("tree", Tier::Utility),
    ("du", Tier::Utility),
    ("sleep", Tier::Utility),
    ("ps", Tier::Utility),
    ("kill", Tier::Utility),
    ("mkdir", Tier::Utility),
    ("cp", Tier::Utility),
    ("mv", Tier::Utility),
    ("rm", Tier::Utility),
    ("touch", Tier::Utility),
    ("chmod", Tier::Utility),
    ("ln", Tier::Utility),
    ("echo", Tier::Utility),
    ("printf", Tier::Utility),
];

/// Map one exact name to the list's own string; paths and unknown names are rejected.
pub fn listed(name: &str) -> Option<&'static str> {
    PROGRAMS.iter().find(|(entry, _)| *entry == name).map(|(entry, _)| *entry)
}

/// Only listed names cross the SQL boundary. The borrowed input lives within this call;
/// NULL, other storage classes and invalid UTF-8 return NULL without a text-bearing error.
pub fn register(db: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::{functions::FunctionFlags, types::ValueRef};
    db.create_scalar_function("t3_programs", 1, FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS, |ctx| {
        let result = match ctx.get_raw(0) {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok().and_then(|text| {
                let names = programs(text);
                if names.is_empty() { None } else { Some(names.join(" ")) }
            }),
            _ => None,
        };
        Ok(result)
    })
}

/// Listed programs in encounter order, each once. Only the list's own strings escape.
pub fn programs(command: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    parse(command, 0, &mut out);
    out
}

/// First program of the highest tier, or None. Unknown caller-supplied names are ignored.
pub fn label(programs: &[&'static str]) -> Option<&'static str> {
    programs.iter().filter_map(|p| PROGRAMS.iter().find(|(name, _)| name == p))
        .min_by_key(|(_, tier)| *tier).map(|(name, _)| *name)
}

#[derive(Default)]
struct Word {
    text: String,
    quoted: bool,
    dynamic: bool,
    assignment: bool,
    incomplete: bool,
}

// Bytes a word can copy as-is, indexed by quote state and whether expansions count.
// Outside quotes, shell whitespace, word delimiters, quotes and escapes end a run.
// Inside single quotes only the closing quote does. Inside double quotes, escapes
// and the closing quote end a run; expansion markers count except in single quotes.
const fn plain_table(quote: u8, expansions: bool) -> [bool; 256] {
    let mut table = [false; 256];
    let mut index = 0;
    while index < table.len() {
        let byte = index as u8;
        let expansion = expansions && matches!(byte, b'$' | b'`');
        table[index] = match quote {
            0 => !byte.is_ascii_whitespace()
                && !matches!(byte, b';' | b'&' | b'|' | b'(' | b')' | b'{' | b'}' | b'<' | b'>'
                    | b'\\' | b'\'' | b'"')
                && !expansion,
            b'\'' => byte != b'\'',
            _ => !matches!(byte, b'\\' | b'"') && !expansion,
        };
        index += 1;
    }
    table
}

const PLAIN: [[[bool; 256]; 2]; 3] = [
    [plain_table(0, false), plain_table(0, true)],
    [plain_table(b'\'', false), plain_table(b'\'', true)],
    [plain_table(b'"', false), plain_table(b'"', true)],
];

struct Lexer<'a> { bytes: &'a [u8], pos: usize }
impl<'a> Lexer<'a> {
    fn at(&self, s: &[u8]) -> bool { self.bytes[self.pos..].starts_with(s) }

    // Rule 2: $(git), $(echo $(cargo)), and `cargo` are opaque, including their quotes.
    // Bash, Shell Parameter Expansion / Process Substitution: ${...} and <(...),
    // >(...) are expansion words too, never new top-level command positions.
    // An explicit stack avoids recursion on hostile nesting. No expansion is evaluated.
    fn substitution(&mut self) {
        let parameter = self.at(b"${");
        let backtick = self.bytes[self.pos] == b'`';
        self.pos += if backtick { 1 } else { 2 };
        let mut docs = Vec::new();
        let mut stack = vec![if backtick { b'`' } else if parameter { b'}' } else { b')' }];
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            let mode = *stack.last().unwrap();
            if mode == b')' && c == b'#' && (self.pos == 0
                || b" \t\n;|&(".contains(&self.bytes[self.pos - 1])) {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' { self.pos += 1; }
            } else if mode == b')' && self.at(b"<<") && !self.at(b"<<<") {
                let tabs = self.at(b"<<-");
                self.pos += if tabs { 3 } else { 2 };
                while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t') { self.pos += 1; }
                let delimiter = self.word_inner(false);
                docs.push((delimiter.text, tabs, delimiter.quoted));
            } else if mode == b')' && c == b'\n' {
                self.pos += 1;
                self.skip_docs(&mut docs);
            } else if c == b'\\' && mode != b'\'' {
                self.pos = (self.pos + 2).min(self.bytes.len());
            } else if c == mode {
                self.pos += 1;
                stack.pop();
                if stack.is_empty() { return; }
            } else if mode == b'\'' {
                self.pos += 1;
            } else if (self.at(b"$(") || self.at(b"${")) && mode != b'`' {
                self.pos += 2;
                stack.push(if self.bytes[self.pos - 1] == b'{' { b'}' } else { b')' });
            } else if c == b'`' || (mode != b'"' && (c == b'\'' || c == b'"')) {
                self.pos += 1;
                stack.push(c);
            } else if c == b'(' && mode == b')' {
                self.pos += 1;
                stack.push(b')');
            } else { self.pos += 1; }
        }
    }

    // Rule 1: echo "a && cargo" and echo 'x | rg' each have one argument.
    // Bash Reference Manual, Escape Character / Double Quotes: backslash-newline
    // disappears; inside double quotes only $, `, ", \ and newline lose a backslash.
    fn word(&mut self) -> Word { self.word_inner(true) }

    fn word_inner(&mut self, expansions: bool) -> Word {
        let start = self.pos;
        let mut w = Word::default();
        let mut value = Vec::new();
        let mut quote = 0;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            let quote_slot = match quote {
                0 => 0,
                b'\'' => 1,
                _ => 2,
            };
            let plain = &PLAIN[quote_slot][usize::from(expansions)];
            if plain[usize::from(c)] {
                let from = self.pos;
                self.pos += 1;
                while self.pos < self.bytes.len() && plain[usize::from(self.bytes[self.pos])] {
                    self.pos += 1;
                }
                value.extend_from_slice(&self.bytes[from..self.pos]);
                continue;
            }
            if expansions && quote != b'\'' && (self.at(b"${") || (quote == 0 && (self.at(b"<(") || self.at(b">(")))) {
                w.dynamic = true;
                value.push(b'?');
                self.substitution();
                continue;
            }
            if quote == 0 && (c.is_ascii_whitespace() || b";&|(){}<>".contains(&c)) { break; }
            if c == b'\\' && quote != b'\'' {
                w.quoted = true;
                self.pos += 1;
                if self.pos == self.bytes.len() { w.dynamic = true; break; }
                let next = self.bytes[self.pos];
                if quote == 0 || b"$`\"\\\n".contains(&next) {
                    if next != b'\n' { value.push(next); }
                    self.pos += 1;
                } else { value.push(c); }
            } else if (c == b'\'' || c == b'"') && (quote == 0 || quote == c) {
                w.quoted = true;
                quote = if quote == 0 { c } else { 0 };
                self.pos += 1;
            } else if expansions && quote != b'\'' && (self.at(b"$(") || c == b'`') {
                w.dynamic = true;
                value.push(b'?');
                self.substitution();
            } else {
                // Expansion-produced executable names are unknowable without execution.
                if expansions && quote != b'\'' && c == b'$' { w.dynamic = true; }
                value.push(c);
                self.pos += 1;
            }
        }
        if quote != 0 { w.dynamic = true; w.incomplete = true; } // Unfinished input never invents a name.
        // Bash, Simple Command Expansion: quoting the assignment name makes it
        // a command word; quoting only its value does not (A='x' cargo).
        w.assignment = std::str::from_utf8(&self.bytes[start..self.pos]).ok().is_some_and(assignment);
        w.text = String::from_utf8_lossy(&value).into_owned();
        w
    }

    fn skip_docs(&mut self, docs: &mut Vec<(String, bool, bool)>) {
        for (delimiter, tabs, quoted) in docs.drain(..) {
            while self.pos < self.bytes.len() {
                let mut logical = String::new();
                loop {
                    let start = self.pos;
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' { self.pos += 1; }
                    let mut line = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
                    if tabs { line = line.trim_start_matches('\t'); }
                    // Bash, Here Documents: unquoted backslash-newline is
                    // joined before checking the delimiter (E\\\nOF -> EOF).
                    let slashes = line.bytes().rev().take_while(|c| *c == b'\\').count();
                    let join = !quoted && slashes % 2 == 1 && self.pos < self.bytes.len();
                    logical.push_str(if join { &line[..line.len() - 1] } else { line });
                    if self.pos < self.bytes.len() { self.pos += 1; }
                    if !join || self.pos == self.bytes.len() { break; }
                }
                if logical == delimiter { break; }
            }
        }
    }
}

fn parse(command: &str, depth: usize, out: &mut Vec<&'static str>) {
    let mut l = Lexer { bytes: command.as_bytes(), pos: 0 };
    let mut words: Vec<Word> = Vec::new();
    let mut docs: Vec<(String, bool, bool)> = Vec::new();
    while l.pos < l.bytes.len() {
        let c = l.bytes[l.pos];
        if c == b'\n' || b";&|(){}".contains(&c) {
            // Rule 3: cd x && cargo | tail; (git) { rg; } start new commands.
            command_words(&words, depth, out);
            words.clear();
            l.pos += 1;
            if c == b'\n' {
                // Rule 2: cat <<'EOF' skips cargo in its body; <<- strips tabs.
                // Bash, Here Documents: queued heredocs are read in redirection order.
                l.skip_docs(&mut docs);
            }
        } else if c.is_ascii_whitespace() { l.pos += 1; }
        else if c == b'#' {
            // Rule 1: # cargo is a comment, but echo x#cargo is a word.
            while l.pos < l.bytes.len() && l.bytes[l.pos] != b'\n' { l.pos += 1; }
        } else if l.at(b"<(") || l.at(b">(") { words.push(l.word()); }
        else if c == b'<' || c == b'>' {
            // Bash, Simple Command Expansion / Redirections: 2>&1 is not a chain,
            // and >cargo git runs git, not cargo. Remove an adjacent IO number.
            if l.pos > 0 && l.bytes[l.pos - 1].is_ascii_digit() && words.last().is_some_and(|w| !w.quoted && !w.text.is_empty()
                && w.text.bytes().all(|b| b.is_ascii_digit())) { words.pop(); }
            let doc = l.at(b"<<") && !l.at(b"<<<");
            let tabs = l.at(b"<<-");
            l.pos += 1;
            if l.pos < l.bytes.len() && l.bytes[l.pos] == c { l.pos += 1; }
            if l.pos < l.bytes.len() && b"&|-<>".contains(&l.bytes[l.pos]) { l.pos += 1; }
            while l.pos < l.bytes.len() && matches!(l.bytes[l.pos], b' ' | b'\t') { l.pos += 1; }
            let target = l.word_inner(!doc);
            if doc { docs.push((target.text, tabs, target.quoted)); }
        } else { words.push(l.word()); }
    }
    command_words(&words, depth, out);
}

fn assignment(s: &str) -> bool {
    s.split_once('=').is_some_and(|(name, _)| !name.is_empty()
        && name.bytes().enumerate().all(|(i, c)| c == b'_' || c.is_ascii_alphabetic()
            || (i > 0 && c.is_ascii_digit())))
}

fn command_words(words: &[Word], depth: usize, out: &mut Vec<&'static str>) {
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        let s = w.text.as_str();
        // Rule 4: A=1 sudo env B=2 timeout 60 make -> make; if cargo -> cargo.
        if w.assignment || (!w.quoted && matches!(s,
            "!" | "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "time")) {
            i += 1; continue;
        }
        if !w.quoted && matches!(s, "for" | "case" | "select" | "function") { return; }
        if w.dynamic { return; }
        // Rule 6: /usr/bin/git counts; ./never-read does not.
        let base = s.rsplit('/').next().unwrap_or(s);
        if matches!(base, "sudo" | "env" | "command" | "exec" | "nohup" | "nice" | "stdbuf" | "setsid" | "timeout") {
            i += 1;
            // Prefix options have utility-specific operands (not shell syntax).
            // Consume common short/long operand options, attached values and --.
            while i < words.len() {
                let flag = words[i].text.as_str();
                if flag == "--" { i += 1; break; }
                if !flag.starts_with('-') || flag == "-" { break; }
                let operand = match base {
                    "sudo" => matches!(flag, "-u" | "-g" | "-h" | "-p" | "-C" | "-T" | "-R" | "-D" | "-r" | "-t" | "--user" | "--group" | "--host" | "--prompt" | "--chdir" | "--chroot" | "--role" | "--type" | "--close-from" | "--command-timeout"),
                    "env" => matches!(flag, "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0"),
                    "exec" => flag == "-a",
                    "nice" => matches!(flag, "-n" | "--adjustment"),
                    "stdbuf" => matches!(flag, "-i" | "-o" | "-e" | "--input" | "--output" | "--error"),
                    "timeout" => matches!(flag, "-k" | "--kill-after" | "-s" | "--signal"),
                    _ => false,
                };
                i += if operand { 2 } else { 1 };
            }
            if base == "timeout" { i += 1; }
            continue;
        }
        // Rule 5: bash -l -c 'cargo' unwraps, bash build.sh does not.
        // Two wrapper edges maximum; a third wrapper contributes no program.
        if matches!(base, "bash" | "sh" | "zsh" | "dash" | "fish") {
            if depth == 2 { return; }
            i += 1;
            while i < words.len() {
                let flag = &words[i];
                if flag.dynamic || !flag.text.starts_with('-') || flag.text == "--" { return; }
                if !flag.text.starts_with("--") && flag.text.contains('c') {
                    if let Some(script) = words.get(i + 1).filter(|w| !w.incomplete) {
                        parse(&script.text, depth + 1, out);
                    }
                    return;
                }
                i += 1;
            }
            return;
        }
        if let Some(name) = listed(base) {
            if !out.contains(&name) { out.push(name); }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(command: &str, names: &[&str], expected_label: Option<&str>) {
        let got = programs(command);
        assert_eq!(got, names, "synthetic command: {command:?}");
        assert_eq!(label(&got), expected_label, "synthetic command: {command:?}");
        for name in got {
            assert!(PROGRAMS.iter().any(|(listed, _)| std::ptr::eq(name, *listed)),
                "result must be the list's own string");
        }
    }

    #[test]
    fn spec_examples() {
        for (command, names, selected) in [
            ("cargo test -p x", vec!["cargo"], Some("cargo")),
            ("/usr/bin/git log", vec!["git"], Some("git")),
            (r#"bash -lc "cd /x && cargo test 2>&1 | tail -20""#, vec!["cargo", "tail"], Some("cargo")),
            ("cd x && FOO=1 python3 run.py", vec!["python3"], Some("python3")),
            ("mkdir -p out && cargo build --release", vec!["mkdir", "cargo"], Some("cargo")),
            ("export A=1; npm test", vec!["npm"], Some("npm")),
            ("rg -n foo | head", vec!["rg", "head"], Some("rg")),
            (r#"echo "a && cargo test""#, vec!["echo"], Some("echo")),
            ("echo 'x | rg y'", vec!["echo"], Some("echo")),
            ("cat <<EOF > f.sh\ncargo test\nEOF\ngit status", vec!["cat", "git"], Some("git")),
            ("x=$(git rev-parse HEAD); echo $x", vec!["echo"], Some("echo")),
            ("echo `cargo --version`", vec!["echo"], Some("echo")),
            ("for f in *.rs; do rustc $f; done", vec!["rustc"], Some("rustc")),
            ("if cargo check; then git commit; fi", vec!["cargo", "git"], Some("cargo")),
            ("sudo -E env A=1 timeout 60 make test", vec!["make"], Some("make")),
            ("(cd x && go test ./...) && git diff", vec!["go", "git"], Some("go")),
            (r#"sh -c 'bash -c "sh -c cargo"'"#, vec![], None),
            ("bash build.sh", vec![], None),
            ("./never-read --flag", vec![], None),
            ("never-read-program arg", vec![], None),
            ("# cargo test", vec![], None),
            ("", vec![], None),
        ] { check(command, &names, selected); }
    }

    #[test]
    fn quoting_comments_and_expansions() {
        check("'A=1' cargo; A='x' git", &["git"], Some("git"));
        check("echo ${never:-cargo;git}; cat <(cargo;git); rg", &["echo", "cat", "rg"], Some("rg"));
        check(r#"bash -c "echo $never; cargo""#, &["echo", "cargo"], Some("cargo"));
        check("echo x#cargo # git\nrg x", &["echo", "rg"], Some("rg"));
        check("ca\\rgo test; g'i't status; \"rg\" x", &["cargo", "git", "rg"], Some("cargo"));
        check("car\\\ngo test", &["cargo"], Some("cargo"));
        check("echo x\\;cargo; git", &["echo", "git"], Some("git"));
        check("echo $(echo $(cargo); git) `rg`; ls", &["echo", "ls"], Some("echo"));
        check("$(cargo) git; $never/cargo; 'cargo", &[], None);
        check("echo \"$(printf ')'; git)\"; rg", &["echo", "rg"], Some("rg"));
        check("echo 'unterminated && cargo", &["echo"], Some("echo"));
        check("echo $(# )\ncargo\n); git", &["echo", "git"], Some("git"));
        check("echo $(cat <<EOF\n)\ncargo\nEOF\n); git", &["echo", "git"], Some("git"));
        check("echo $(unterminated; cargo", &["echo"], Some("echo"));
        check("bash -c 'echo \"a\\$never\"; git'", &["echo", "git"], Some("git"));
    }

    #[test]
    fn redirections_and_heredocs() {
        check(">cargo 2>&1 git log; cargo 2>>out |& tail", &["git", "cargo", "tail"], Some("cargo"));
        check("cat <<'ONE' <<-\"TWO\"\ncargo\nONE\n\tgit\n\tTWO\nrg", &["cat", "rg"], Some("rg"));
        check("cat <<EOF\ncargo\nE\\\nOF\ngit", &["cat", "git"], Some("git"));
        check("cat <<EOF\ncargo; git", &["cat"], Some("cat"));
        check("cat <<< 'cargo; git'; rg", &["cat", "rg"], Some("rg"));
        check("cat <<E\\OF\ncargo\nEOF\ngit", &["cat", "git"], Some("git"));
    }

    #[test]
    fn prefixes_control_flow_and_shell_depth() {
        check("! if A=1 sudo -u never env -u never B=2 nice -n 4 stdbuf -o L setsid timeout -k 1 -s TERM 60 /bin/make", &["make"], Some("make"));
        check("command -- exec -a never nohup cargo", &["cargo"], Some("cargo"));
        check("while git diff; do cargo test; done", &["git", "cargo"], Some("cargo"));
        check("case cargo in never) echo ok;; esac; select git in cargo; do ls; done; function cargo() { rg; }", &["echo", "ls", "rg"], Some("rg"));
        check("bash -l -c 'sh -ec cargo'", &["cargo"], Some("cargo"));
        check("dash -ic git; zsh -ec rg; fish -c cargo", &["git", "rg", "cargo"], Some("cargo"));
        check("bash never cargo; env -- never git", &[], None);
        check("{ git; } & cargo || rg && ls\nprintf x", &["git", "cargo", "rg", "ls", "printf"], Some("cargo"));
    }

    #[test]
    fn unknown_program_never_leaves_parser() {
        check("never-read-program arg", &[], None);
        check("./never-read arg", &[], None);
    }

    #[test]
    fn list_identity_order_dedup_and_tiers() {
        for (name, tier) in PROGRAMS {
            let owned = format!("/synthetic/{name} never");
            let got = programs(&owned);
            assert_eq!(got.len(), 1);
            assert!(std::ptr::eq(got[0], *name));
            assert_eq!(label(&got), Some(*name));
            assert!(PROGRAMS.iter().filter(|(n, _)| n == name).count() == 1);
            assert!((1..=3).contains(&(*tier as u8)));
        }
        check("git; cargo; npm; cargo; git; echo", &["git", "cargo", "npm", "echo"], Some("cargo"));
        check("tail; echo; rg; git", &["tail", "echo", "rg", "git"], Some("rg"));
        check("printf; echo", &["printf", "echo"], Some("printf"));
        assert_eq!(label(&["never"]), None);
    }

    #[test]
    fn seeded_fuzz_only_static_list_entries() {
        let mut seed = 0x7472_6163_6532_u64;
        let mut next = || {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed
        };
        let punctuation = [" ", "\n", "\t", "'", "\"", "\\", "$(", ")", "`", "&&", "||", "|", ";", "&", "(", "{", "}", "<<", "<<-", "<<<", "#", "=", "never", "é", "🦀"];
        for _ in 0..6000 {
            let mut input = String::new();
            for _ in 0..(next() % 160) {
                let n = next() as usize % (punctuation.len() + PROGRAMS.len());
                input.push_str(if n < punctuation.len() { punctuation[n] } else { PROGRAMS[n - punctuation.len()].0 });
            }
            let got = programs(&input);
            for name in &got {
                assert!(PROGRAMS.iter().any(|(listed, _)| std::ptr::eq(*name, *listed)));
            }
            assert_eq!(got.len(), got.iter().collect::<std::collections::HashSet<_>>().len());
        }
        // Deep substitution nesting must not exhaust the call stack.
        check(&format!("echo {}cargo{}; git", "$(".repeat(10000), ")".repeat(10000)), &["echo", "git"], Some("git"));
    }

    fn synthetic_commands() -> Vec<String> {
        (0..18000).map(|i| {
            let chain = match i % 3 {
                0 => "mkdir -p out && cargo build",
                1 => "git diff | rg never | head",
                _ => "mkdir out; cp never out; npm test | tail",
            };
            let doc = if i % 100 == 0 {
                format!("cat <<EOF\n{}EOF\n", "never cargo; git\n".repeat(300))
            } else { String::new() };
            format!("bash -lc 'cd /synthetic && {doc}{chain}'")
        }).collect()
    }

    struct SyntheticDb(std::path::PathBuf);
    impl SyntheticDb {
        fn new(commands: &[String]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("t3-program-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let fixture = Self(path);
            let mut db = rusqlite::Connection::open(fixture.path()).unwrap();
            db.execute_batch("CREATE TABLE commands(at INTEGER, input); CREATE VIEW names AS SELECT t3_programs(input) AS names FROM commands;").unwrap();
            let tx = db.transaction().unwrap();
            {
                let mut insert = tx.prepare("INSERT INTO commands VALUES (?1, ?2)").unwrap();
                for (i, command) in commands.iter().enumerate() {
                    insert.execute(rusqlite::params![i as i64, command]).unwrap();
                }
            }
            tx.commit().unwrap();
            fixture
        }
        fn path(&self) -> std::path::PathBuf { self.0.join("synthetic.sqlite") }
    }
    impl Drop for SyntheticDb {
        fn drop(&mut self) {
            std::fs::remove_file(self.path()).unwrap();
            std::fs::remove_dir(&self.0).unwrap();
        }
    }

    #[test]
    fn listed_returns_only_list_owned_strings() {
        for (name, _) in PROGRAMS {
            let owned = name.to_string();
            assert!(std::ptr::eq(listed(&owned).unwrap(), *name));
        }
        for name in ["", "never", "/bin/git", "git arg", "GIT"] { assert_eq!(listed(name), None); }
    }

    #[test]
    fn sql_boundary_is_read_only_and_returns_only_names() {
        let commands = vec!["mkdir out && cargo test | tail".into(), "never-read arg".into(),
            "bash -lc 'cd /synthetic && git status'".into(), "echo 'cargo;git'".into()];
        let fixture = SyntheticDb::new(&commands);
        let db = crate::recap::open(&fixture.path()).unwrap();
        register(&db).unwrap();
        assert_eq!(db.query_row("PRAGMA query_only", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        assert!(db.execute("INSERT INTO commands VALUES (42, 'never')", []).is_err());
        assert!(db.execute("CREATE TABLE forbidden(x)", []).is_err());
        let flags: i64 = db.query_row("SELECT flags FROM pragma_function_list WHERE name='t3_programs' AND narg=1", [], |r| r.get(0)).unwrap();
        assert_eq!(flags & (0x800 | 0x200000), 0x800 | 0x200000);
        db.execute_batch("PRAGMA trusted_schema=OFF").unwrap();
        let _: Option<String> = db.query_row("SELECT names FROM names LIMIT 1", [], |r| r.get(0)).unwrap();
        let mut stmt = db.prepare("SELECT at, t3_programs(input) FROM commands ORDER BY rowid").unwrap();
        let got: Vec<Option<String>> = stmt.query_map([], |r| r.get(1)).unwrap().map(Result::unwrap).collect();
        let expected: Vec<Option<String>> = commands.iter().map(|c| {
            let names = programs(c);
            if names.is_empty() { None } else { Some(names.join(" ")) }
        }).collect();
        assert_eq!(got, expected);
        for value in ["NULL", "17", "1.5", "x'636172676f'", "CAST(x'80ff' AS TEXT)"] {
            let result: Option<String> = db.query_row(&format!("SELECT t3_programs({value})"), [], |r| r.get(0)).unwrap();
            assert_eq!(result, None);
        }
        for result in got.into_iter().flatten() {
            for name in result.split(' ') { assert!(listed(name).is_some()); }
        }
    }

    #[test]
    #[ignore]
    fn measure_sql_18000_rows_five_alternating_trials() {
        let commands = synthetic_commands();
        let fixture = SyntheticDb::new(&commands);
        let db = crate::recap::open(&fixture.path()).unwrap();
        register(&db).unwrap();
        let mut baseline = db.prepare("SELECT at FROM commands ORDER BY rowid").unwrap();
        let mut parsed = db.prepare("SELECT at, t3_programs(input) FROM commands ORDER BY rowid").unwrap();
        // Validate every row outside timing; raw command text never leaves SQLite.
        {
            let mut rows = parsed.query([]).unwrap();
            for command in &commands {
                let row = rows.next().unwrap().unwrap();
                assert_eq!(row.get::<_, String>(1).unwrap(), programs(command).join(" "));
            }
            assert!(rows.next().unwrap().is_none());
        }
        let mut trials = [Vec::new(), Vec::new()];
        for _ in 0..5 {
            for side in 0..2 {
                let start = std::time::Instant::now();
                let mut rows = if side == 0 { baseline.query([]).unwrap() } else { parsed.query([]).unwrap() };
                let mut count = 0;
                let mut stamps = 0_i64;
                let mut names = 0;
                while let Some(row) = rows.next().unwrap() {
                    count += 1;
                    stamps += std::hint::black_box(row.get::<_, i64>(0).unwrap());
                    if side == 1 {
                        let value = std::hint::black_box(row.get::<_, String>(1).unwrap());
                        names += value.split(' ').count();
                    }
                }
                trials[side].push(start.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(count, 18000);
                assert_eq!(stamps, 161991000);
                if side == 1 { assert_eq!(names, 54180); }
            }
        }
        for (side, mut times) in trials.into_iter().enumerate() {
            let order = times.clone();
            times.sort_by(f64::total_cmp);
            println!("SQL side {side} (0 timestamp, 1 t3_programs), ms: {order:?}; median {:.3}, range {:.3}..{:.3}", times[2], times[0], times[4]);
        }
    }

    // Synthetic, parsing only; no database, shell execution or filesystem access.
    // Run explicitly with --ignored --nocapture. Construction is outside timing.
    #[test]
    #[ignore]
    fn measure_18000_commands_five_trials() {
        let corpus = synthetic_commands();
        for (i, command) in corpus.iter().enumerate() {
            let mut expected = if i % 100 == 0 { vec!["cat"] } else { Vec::new() };
            expected.extend(match i % 3 {
                0 => vec!["mkdir", "cargo"],
                1 => vec!["git", "rg", "head"],
                _ => vec!["mkdir", "cp", "npm", "tail"],
            });
            assert_eq!(programs(command), expected);
        }
        let mut trials = Vec::new();
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut count = 0;
            for command in &corpus {
                count += std::hint::black_box(programs(std::hint::black_box(command))).len();
            }
            trials.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(count, 54180); // 2/3/4 names per chain, plus 180 heredoc cats.

        }
        let order = trials.clone();
        trials.sort_by(f64::total_cmp);
        println!("18000 synthetic commands, ms: {order:?}; median {:.3}, range {:.3}..{:.3}", trials[2], trials[0], trials[4]);
    }
}

#[cfg(test)]
mod word_scan {
    use super::*;

    // The byte-at-a-time word scanner from v1.4.0.
    impl<'a> Lexer<'a> {
        fn word_inner_old(&mut self, expansions: bool) -> Word {
            let start = self.pos;
            let mut w = Word::default();
            let mut value = Vec::new();
            let mut quote = 0;
            while self.pos < self.bytes.len() {
                let c = self.bytes[self.pos];
                if expansions && quote != b'\'' && (self.at(b"${") || (quote == 0 && (self.at(b"<(") || self.at(b">(")))) {
                    w.dynamic = true;
                    value.push(b'?');
                    self.substitution();
                    continue;
                }
                if quote == 0 && (c.is_ascii_whitespace() || b";&|(){}<>".contains(&c)) { break; }
                if c == b'\\' && quote != b'\'' {
                    w.quoted = true;
                    self.pos += 1;
                    if self.pos == self.bytes.len() { w.dynamic = true; break; }
                    let next = self.bytes[self.pos];
                    if quote == 0 || b"$`\"\\\n".contains(&next) {
                        if next != b'\n' { value.push(next); }
                        self.pos += 1;
                    } else { value.push(c); }
                } else if (c == b'\'' || c == b'"') && (quote == 0 || quote == c) {
                    w.quoted = true;
                    quote = if quote == 0 { c } else { 0 };
                    self.pos += 1;
                } else if expansions && quote != b'\'' && (self.at(b"$(") || c == b'`') {
                    w.dynamic = true;
                    value.push(b'?');
                    self.substitution();
                } else {
                    // Expansion-produced executable names are unknowable without execution.
                    if expansions && quote != b'\'' && c == b'$' { w.dynamic = true; }
                    value.push(c);
                    self.pos += 1;
                }
            }
            if quote != 0 { w.dynamic = true; w.incomplete = true; } // Unfinished input never invents a name.
            // Bash, Simple Command Expansion: quoting the assignment name makes it
            // a command word; quoting only its value does not (A='x' cargo).
            w.assignment = std::str::from_utf8(&self.bytes[start..self.pos]).ok().is_some_and(assignment);
            w.text = String::from_utf8_lossy(&value).into_owned();
            w
        }
    }

    fn compare(command: &str, start: usize) {
        for expansions in [false, true] {
            let mut old = Lexer { bytes: command.as_bytes(), pos: start };
            let mut new = Lexer { bytes: command.as_bytes(), pos: start };
            let expected = old.word_inner_old(expansions);
            let actual = new.word_inner(expansions);
            let fields = |word: Word| {
                (word.text, word.quoted, word.dynamic, word.assignment, word.incomplete)
            };
            assert_eq!(
                (new.pos, fields(actual)), (old.pos, fields(expected)),
                "command {command:?}, start {start}, expansions {expansions}",
            );
        }
    }

    #[test]
    fn every_utf8_byte_pair_in_each_quote_state() {
        let mut bytes = [0; 3];
        for prefix in [None, Some(b'\''), Some(b'"')] {
            bytes[0] = prefix.unwrap_or_default();
            let offset = usize::from(prefix.is_none());
            for first in 0..=u8::MAX {
                bytes[1] = first;
                for second in 0..=u8::MAX {
                    bytes[2] = second;
                    if let Ok(command) = std::str::from_utf8(&bytes[offset..]) {
                        compare(command, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn seeded_utf8_commands_match_the_byte_at_a_time_scan() {
        const PIECES: &[&str] = &[
            "a", "git", "cargo", " ", "\t", "\n", "\x0c", "\r", ";", "&", "|", "(", ")", "{", "}",
            "<", ">", "\\", "'", "\"", "`", "$", "${", "$(", "<(", ">(", "#", "=", "A=", "0",
            "-c", "é", "😀", "<<", "EOF", "x/y", ".", "~", "$(cat <<EOF\né\nEOF\n)",
        ];
        // Xorshift64* with a fixed seed so failures can be reproduced.
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut below = |limit: usize| {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            ((seed.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as usize) % limit
        };
        let mut command = String::new();
        for _ in 0..30_000 {
            command.clear();
            for _ in 0..below(18) {
                if below(3) == 0 {
                    command.push_str(PIECES[below(PIECES.len())]);
                } else {
                    for _ in 0..=below(8) {
                        command.push(char::from(b'a' + below(26) as u8));
                    }
                }
            }
            for (start, _) in command.char_indices().take(24) {
                compare(&command, start);
            }
        }
    }
}
