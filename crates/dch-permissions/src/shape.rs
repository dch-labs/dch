//! The shape parsers: reducing a tool call to what the policy can read.
//!
//! A Bash command becomes reader segments and verb prefixes; a write
//! call becomes the targets it would overwrite; the session's own
//! furniture is recognized by name and operation. Everything here is
//! pure — strings and JSON in, judgments and rules out — and everything
//! the permission system knows about a call's *content* is derived by
//! this module, so the reader gate, the remember computation, and the
//! write-target gate all speak one truth about a call.

use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

use crate::rule::RememberRules;

/// The placeholder a quoted span becomes under masking.
///
/// NUL is the one byte a real command line can never carry — `execve`
/// forbids it in argv — so the token cannot be spoofed by input, and a
/// flag policy can refuse it on sight.
const QUOTED_ARG: &str = "\u{0}";

/// The arity table: how many leading tokens name a command's verb.
///
/// `cargo test -p x` is `cargo test`, `npm run dev` is `npm run`; an
/// unknown program names itself alone. Longest match wins, so
/// `cargo run --bin x` takes the arity-3 `cargo run` entry (prefix
/// `cargo run --bin`) over the two-token `cargo`.
const VERB_ARITY: &[(&str, usize)] = &[
    ("npm run", 3),
    ("npm exec", 3),
    ("pnpm run", 3),
    ("yarn run", 3),
    ("cargo run", 3),
    ("cargo add", 3),
    ("docker compose", 3),
    ("cargo", 2),
    ("git", 2),
    ("gh", 2),
    ("npm", 2),
    ("pnpm", 2),
    ("yarn", 2),
    ("go", 2),
    ("make", 2),
    ("docker", 2),
    ("kubectl", 2),
    ("systemctl", 2),
];

/// Mask quoted spans so literal text cannot act as — or masquerade
/// as — shell control.
///
/// A single-quoted span is wholly literal: it becomes one
/// [`QUOTED_ARG`] token, delimiters gone — a quoted `|` is not a
/// separator, a quoted `>` is not a redirect, a quoted `\` is text. A
/// double-quoted span is literal except that substitution reaches
/// inside it, and only the execution-capable forms matter to the
/// mask: a `$(` or `${` inside the span — or a backtick, which
/// executes wherever it sits — pushes the opener pair itself, the
/// marker every later judgment reads as execution, because the span
/// walls hide the characters that would otherwise betray it. A span
/// whose only expansion is a bare `$NAME` or `$?` prints a variable
/// and touches nothing, so it masks as the plain placeholder like
/// any other quoted argument — `echo "exit: $?"` reads as
/// `echo <arg>`, prefixable and coverable, while `echo "$(rm)"`
/// keeps its execution visible. Escapes inside double quotes are
/// consumed in pairs, as the shell reads them. Outside quotes a
/// backslash escapes the next character: the pair becomes that
/// character plainly, or a space when the character is one the gate
/// treats as control — so `find`'s `\( … \)` grouping reads as
/// text, and `\;` cannot smuggle a separator.
fn mask_quoted(command: &str) -> String {
    let mut masked = String::with_capacity(command.len());
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                }
                masked.push_str(QUOTED_ARG);
            }
            '"' => {
                let mut executes = false;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            chars.next();
                        }
                        '`' => executes = true,
                        '$' => {
                            if matches!(chars.peek(), Some('(' | '{')) {
                                executes = true;
                            }
                        }
                        _ => {}
                    }
                }
                if executes {
                    masked.push_str("$(");
                } else {
                    masked.push_str(QUOTED_ARG);
                }
            }
            '\\' => match chars.next() {
                Some(ch) if "><$`();|&\\".contains(ch) || ch.is_whitespace() => {
                    masked.push(' ');
                }
                Some(ch) => masked.push(ch),
                None => masked.push(' '),
            },
            other => masked.push(other),
        }
    }
    masked
}

/// The inert-redirect spellings the scrub removes.
///
/// `2>/dev/null` ends with `>/dev/null`, so the table's order is
/// load-bearing: the first spelling that matches a position claims
/// it, and the digit form must see the position before the bare
/// operator form can eat its tail.
const INERT_REDIRECTS: &[&str] = &["2>/dev/null", ">/dev/null", "2>&1", "1>&2"];

/// Whether one side of a byte position is a shell token boundary.
///
/// An inert spelling may be scrubbed only where the shell itself
/// would read it as the redirect: bounded by nothing, whitespace, or
/// a control operator. Any other byte on either side — the `x` before
/// in `x2>/dev/nullbar`, the `.` after in `2>/dev/null.txt` — glues
/// the spelling into a word, and a word that opens a file is a writer
/// the gate must keep seeing.
fn token_boundary(before: Option<&u8>, after: Option<&u8>) -> bool {
    let edge = |side: Option<&u8>| {
        side.is_none_or(|byte| byte.is_ascii_whitespace() || b";|&()".contains(byte))
    };
    edge(before) && edge(after)
}

/// Remove the inert-redirect spellings that stand as their own token.
///
/// Discarding or rerouting diagnostics touches no file, so the four
/// spellings go — but only where they stand alone between token
/// boundaries, and only outside quotes, where the letters are literal
/// text rather than an operator. A spelling glued into a word stays,
/// and the `>` it carries keeps the command out of the reader gate;
/// the quote walk mirrors the mask's own states: single-quoted bytes
/// literal, double-quoted escapes consumed in pairs.
fn scrub(command: &str) -> String {
    let bytes = command.as_bytes();
    let mut removals: Vec<(usize, usize)> = Vec::new();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if let Some(open) = quote {
            if open == b'"' && byte == b'\\' {
                // an escaped byte inside double quotes: the pair is one
                i = i.saturating_add(2);
                continue;
            }
            if byte == open {
                quote = None;
            }
            i = i.saturating_add(1);
        } else if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            i = i.saturating_add(1);
        } else {
            let mut step = i.saturating_add(1);
            for spelling in INERT_REDIRECTS {
                let Some(end) = i.checked_add(spelling.len()) else {
                    continue;
                };
                if bytes.get(i..end) == Some(spelling.as_bytes())
                    && token_boundary(
                        i.checked_sub(1).and_then(|prior| bytes.get(prior)),
                        bytes.get(end),
                    )
                {
                    removals.push((i, end));
                    step = end;
                    break;
                }
            }
            i = step;
        }
    }
    let mut out = String::with_capacity(command.len());
    let mut skip_until = 0usize;
    let mut pending = removals.iter();
    let mut upcoming = pending.next().copied();
    for (idx, ch) in command.char_indices() {
        if let Some((start, end)) = upcoming
            && idx == start
        {
            skip_until = end;
            upcoming = pending.next().copied();
            continue;
        }
        if idx >= skip_until {
            out.push(ch);
        }
    }
    out
}

fn scrub_and_mask(command: &str) -> String {
    mask_quoted(&scrub(command))
}

/// Whether a masked span carries a substitution that can *execute*.
///
/// bash draws the line the *remembering* judgment must draw: a `$`
/// naming a variable or a parameter — `$?`, `$NAME`, `$1` — only
/// expands, so the segment keeps its verb prefix (`echo $?` is the
/// `echo` verb, remembered and covered like any echo), while `$(`
/// opens a command substitution, `${` opens a parameter expansion
/// that can embed one (`${x:-$(rm)}`), and a backtick substitutes
/// wherever it sits — those spellings make the segment exact-text
/// only, because no prefix can name what they will run. The reader
/// gate draws the stricter line — every unquoted `$` disqualifies
/// there, because the expansion word-splits into new words and flags
/// — so this judgment is the complexity side alone; see
/// [`shell_reads_only`].
fn substitution_executes(span: &str) -> bool {
    span.contains('`') || span.contains("$(") || span.contains("${")
}

/// Whether a shell command only looks, never touches.
///
/// Conservative by construction: a single line, and no character that
/// could redirect, substitute, or split — `>`, `<`, parens,
/// backslash, backtick, or any `$` — anywhere outside quotes; those
/// disqualify the whole command. Every `$` is refused, not only the
/// executing spellings, because bash word-splits the expansion's
/// result: a bare `$NAME` becomes new words and flags — `find
/// ./sandbox $ACTION` is whatever `ACTION` holds, `-delete` included
/// — so no unquoted expansion reads. Two spans cannot disqualify: the
/// inert redirects (`2>/dev/null`, `>/dev/null`, `2>&1`, `1>&2`) are
/// scrubbed first — each spelling only where it stands as its own
/// token, never glued into a word, where its `>` opens a real file —
/// because discarding or rerouting diagnostics touches no file, and
/// quoted text is masked (a single- or double-quoted span collapses to
/// one opaque token), because literal characters cannot redirect,
/// substitute, separate, or re-split — a grep pattern's `\+` stays a
/// pattern, and a quoted `$?` stays what the shell hands the echo
/// whole (`echo "exit: $?"` reads). A real `> real-file` keeps its
/// `>` and still disqualifies. The chaining characters (`;`, `|`,
/// `&`) split the masked line into segments — empty segments, the
/// artifacts of `&&`, are dropped — and **every** segment must be a
/// command from the known-reading list: a pipeline of readers reads
/// (`rg -n foo |
/// wc -l`), a chain with one writer anywhere does not (`cargo build |
/// wc -l`).
#[must_use]
pub fn shell_reads_only(command: &str) -> bool {
    if command.lines().count() != 1 {
        return false;
    }
    let masked = scrub_and_mask(command);
    if masked.chars().any(|ch| "><`()\\$".contains(ch)) {
        return false;
    }
    masked
        .split(['|', ';', '&'])
        .filter(|segment| !segment.trim().is_empty())
        .all(|segment| segment_reads_only(segment.trim()))
}

/// Whether one command segment — the exact text a [`CommandSegment`]
/// carries — is a reader by the gate's own judgment.
///
/// [`deny_rules`] filters segments through this: a
/// reader is never what was refused, so it mints no deny rule. The
/// same judgment the reader gate consults —
/// quoted spans masked first, a live redirect or substitution
/// anywhere in the segment disqualifying it exactly as the
/// whole-command gate would, then arguments scrutinized (`find . -delete`
/// is no reader; `find . -name x` is) — so there is one truth about
/// reader-ness on both sides of the sheet, not two. Idempotent on
/// already-masked text: the placeholder a quoted span collapses to
/// carries no quotes itself.
#[must_use]
pub fn segment_reads_only(segment: &str) -> bool {
    let masked = mask_quoted(segment);
    if masked.chars().any(|ch| "><`()\\$".contains(ch)) {
        return false;
    }
    command_reads_only(&masked)
}

/// Whether one pipe segment — a bare command and its arguments — is a
/// known reader.
///
/// Admission to the reader list is necessary, never sufficient: the
/// program's arguments must also pass [`reader_arguments_read_only`],
/// which refuses the writing and executing arguments a few readers
/// carry, and `git` is gated to its reading subcommands on top. A
/// path-prefixed program is admitted only from the standard system
/// bin directories (`/usr/bin`, `/bin`, `/usr/local/bin`) — a
/// repository-local or otherwise pathed executable refuses whatever
/// its basename spells, because a repo can ship a binary that merely
/// shares a reader's name. Dot-dot components refuse outright: the
/// kernel resolves them at `execve`, so no prefix admits a traversal
/// spelling.
fn command_reads_only(segment: &str) -> bool {
    let mut words = segment.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    if program.contains('/') {
        if program.split('/').any(|component| component == "..") {
            return false;
        }
        let system_bin = program.starts_with("/usr/bin/")
            || program.starts_with("/bin/")
            || program.starts_with("/usr/local/bin/");
        if !system_bin {
            return false;
        }
    }
    let program = program.rsplit('/').next().unwrap_or(program);
    if !matches!(
        program,
        "ls" | "cat"
            | "head"
            | "tail"
            | "pwd"
            | "wc"
            | "stat"
            | "file"
            | "du"
            | "df"
            | "which"
            | "whoami"
            | "date"
            | "grep"
            | "rg"
            | "find"
            | "diff"
            | "cmp"
            | "env"
            | "printenv"
            | "uname"
            | "id"
            | "groups"
            | "tty"
            | "basename"
            | "dirname"
            | "realpath"
            | "readlink"
            | "more"
            | "less"
            | "column"
            | "cut"
            | "sort"
            | "uniq"
            | "tr"
            | "rev"
            | "nl"
            | "xxd"
            | "od"
            | "hexdump"
            | "strings"
            | "md5sum"
            | "sha1sum"
            | "sha256sum"
            | "seq"
            | "jq"
            | "ps"
            | "lsof"
            | "uptime"
            | "free"
            | "vmstat"
            | "nproc"
            | "lscpu"
            | "lsmod"
            | "w"
            | "who"
            | "expr"
            | "numfmt"
            | "echo"
            | "cd"
            | "sleep"
            | "set"
            | "git"
            | "cargo"
            | "pgrep"
            | "journalctl"
            | "systemctl"
    ) {
        return false;
    }
    let args: Vec<&str> = words.collect();
    reader_arguments_read_only(program, &args)
}

/// Whether a known reader's arguments keep it reading.
///
/// A bare program name is not a verdict: several readers carry
/// arguments that write files, set state, or run other programs —
/// `find -delete`, `sort -o`, `sort --compress-program`, `date -s`,
/// `less -o` and its uppercase twin `-O`, `git log
/// --output`, and `env` followed by anything at all; `rg --pre`
/// runs a command per searched file, `git -c … --ext-diff` runs an
/// external diff driver, and `systemctl -H` reaches a remote host;
/// GNU `uniq`'s *second* positional names an output file, so one
/// file argument is all it keeps, and `xxd`'s second names the hex
/// dump's own output — the same shape. No argument to any reader may start `--output` — the one refusal
/// the whole gate shares. Quoting narrows twice over: a wholly quoted
/// argument refuses for the readers whose flags can execute or write
/// (`env`, `sort`, `date`, `less`, `git`, `cargo`, `rg`) — the mask
/// cannot tell a quoted flag from a quoted pattern — and a token
/// splicing a quoted span into its flag-name portion (`'-de'lete`,
/// `'--pre'=cmd`) refuses for every reader, because the name the
/// program finally reads is not the name this policy compared; only a
/// quoted value after `=` (`--include='*.rs'`) keeps the name literal
/// and the command judged. Long flags are
/// matched exactly or by `=`-prefix; short flags can combine (`sort
/// -ro` is `-r -o`), so a short cluster is refused whenever it
/// carries the letter of a writing flag — crude on purpose, erring
/// the way the whole gate errs: a refused read costs a prompt, a
/// waved-through write costs the file.
fn reader_arguments_read_only(program: &str, args: &[&str]) -> bool {
    // The shared refusal: `--output` names a file to write wherever a
    // program accepts it, so no argument to any reader may start it —
    // in full or as the GNU abbreviation of it.
    if args.iter().any(|arg| abbreviates(arg, "output")) {
        return false;
    }
    // A quoted span collapses to one opaque token before this policy
    // sees it, so a wholly quoted flag cannot be told from a wholly
    // quoted pattern: the readers whose flags can execute or write
    // refuse every quoted argument.
    if args.contains(&QUOTED_ARG)
        && matches!(
            program,
            "env" | "sort" | "date" | "less" | "git" | "cargo" | "rg"
        )
    {
        return false;
    }
    // A token splicing the placeholder into its flag-name portion is
    // worse: the shell concatenates the spans back together
    // (`'-de'lete`, `'--pre'=cmd`), and the flag name the program
    // reads is not the name the checks below compared.
    if args.iter().any(|arg| quote_splices_flag_name(arg)) {
        return false;
    }
    match program {
        "env" => args.is_empty(),
        "set" => set_arguments_read_only(args),
        "find" => find_arguments_read_only(args),
        "rg" => !args.iter().any(|arg| {
            *arg == "--pre"
                || arg.starts_with("--pre=")
                || *arg == "--pager"
                || arg.starts_with("--pager=")
        }),
        "sort" => !args.iter().any(|arg| {
            *arg == "--output"
                || arg.starts_with("--output=")
                || abbreviates(arg, "compress-program")
                || short_cluster(arg, 'o')
        }),
        "date" => !args.iter().any(|arg| {
            *arg == "--set"
                || arg.starts_with("--set=")
                || abbreviates(arg, "set")
                || short_cluster(arg, 's')
        }),
        "less" => !args.iter().any(|arg| {
            arg.starts_with("--log-file")
                || arg.starts_with("--LOG-FILE")
                || abbreviates(arg, "log-file")
                // `-O` is the family's overwrite spelling — the
                // uppercase twin of the `-o` beside it — and the
                // cluster scan refuses either case, so no spelling
                // of the log-file flag rides a cluster past the arm.
                || short_cluster(arg, 'o')
                || short_cluster(arg, 'O')
        }),
        "journalctl" => journalctl_arguments_read_only(args),
        "uniq" => uniq_arguments_read_only(args),
        "xxd" => xxd_arguments_read_only(args),
        "systemctl" => {
            // -H/--host and -M/--machine leave this machine — the
            // command runs through ssh against a remote manager — so
            // their presence refuses whatever the subcommand reads.
            if args.iter().any(|arg| {
                matches!(*arg, "-H" | "--host" | "-M" | "--machine")
                    || arg.starts_with("--host=")
                    || arg.starts_with("--machine=")
            }) {
                return false;
            }
            let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
            matches!(
                subcommand.copied(),
                Some(
                    "status"
                        | "is-active"
                        | "is-enabled"
                        | "show"
                        | "list-units"
                        | "list-unit-files"
                        | "cat"
                        | "help"
                )
            )
        }
        "cargo" => {
            let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
            matches!(
                subcommand.copied(),
                Some("tree" | "version" | "locate-project" | "pkgid")
            )
        }
        "git" => git_arguments_read_only(args),
        _ => true,
    }
}

/// Whether a masked token splices the placeholder into its flag-name
/// portion — the shape the shell reassembles into a flag this policy
/// never compared.
///
/// The mask collapses each quoted span to [`QUOTED_ARG`], so a token
/// carrying the placeholder beside other characters is a splice:
/// `'-de'lete`, `'--pre'=cmd`, and `--'output'=file` all arrive here
/// as one token whose name portion is partly placeholder, and the
/// name the program finally reads is whatever the spans concatenated
/// back to. Only the value half of a `flag='value'` token is safe to
/// mask — the name before the `=` stands literal, so the flag checks
/// read exactly what the program will — and a wholly quoted token is
/// the blanket list's question, not this one's.
fn quote_splices_flag_name(arg: &str) -> bool {
    if arg == QUOTED_ARG {
        return false;
    }
    let Some((before, _)) = arg.split_once(QUOTED_ARG) else {
        return false;
    };
    !before.ends_with('=')
}

/// The `set` policy: the option-setter reads when it only tunes the shell.
///
/// `set -o pipefail` and the other failure-behavior options change how
/// the shell reports errors — no file is touched, no program run — so
/// those exact option names ride like a look, and bare `set` (which
/// prints the shell's variables) reads too. Anything else the builtin
/// can do — `set -a` exports, `set --` re-argues, a short flag, an
/// unknown option — refuses, erring the way the whole gate errs: a
/// refused look costs a prompt, a waved-through write costs the file.
/// The policy matters beyond its own command because the reader gate
/// judges whole chains: a preamble that read as a writer would drag
/// the real work behind it out of the reader carve-out. On the
/// remember side the preamble mints nothing — it is a reader, and
/// readers mint nothing on either side of the sheet — so a chain
/// that grows one re-asks for its writers alone, and the preamble
/// rides free regardless.
fn set_arguments_read_only(args: &[&str]) -> bool {
    if args.is_empty() {
        return true;
    }
    let mut options = args.iter().copied();
    while let Some(arg) = options.next() {
        if arg != "-o" {
            return false;
        }
        match options.next() {
            Some("pipefail" | "errexit" | "nounset" | "posix") => {}
            _ => return false,
        }
    }
    true
}

/// The `git` policy: a reading subcommand, read past the global
/// options that may precede it.
///
/// `git` accepts global options before its subcommand — `-C <path>`,
/// `-C <dir>`, `--no-pager`, `--git-dir <dir>` — so the subcommand
/// gate walks past them to the verb itself (`git -C … log` is `git
/// log`). The globals that name a program git runs refuse outright:
/// `-c` can carry an external diff driver or pager command,
/// `--paginate` and `--pager=…` force a pager, `--ext-diff` turns
/// drivers on (`--no-pager` and `--no-ext-diff` disable and stay
/// walked-past). The value-taking globals (`-C`, `--git-dir`,
/// `--work-tree`) consume their following word too; a dash-flag with
/// its value attached (`--git-dir=…`) is one word and skips on its
/// own. An unlisted value-taking global misreads its value as the
/// subcommand and refuses — conservative, never permissive. The
/// `--output` refusal lives one level up, shared by every reader.
///
/// Beyond the always-reading subcommands, the listing subcommands
/// (`branch`, `tag`, `remote`) read bare or with listing flags only —
/// with other arguments they create, delete, or configure — and
/// `stash` and `worktree` read only through their `list` (and `show`)
/// sub-subcommands. `grep` reads with one refusal: `-O` and
/// `--open-files-in-pager` name the program that opens each match on
/// the command line, so those spellings refuse outright.
fn git_arguments_read_only(args: &[&str]) -> bool {
    let mut takes_value = false;
    let mut subcommand_at = None;
    for (index, arg) in args.iter().enumerate() {
        if takes_value {
            takes_value = false;
        } else if matches!(*arg, "-c" | "--paginate" | "--ext-diff") || arg.starts_with("--pager") {
            return false;
        } else if matches!(*arg, "-C" | "--git-dir" | "--work-tree") {
            takes_value = true;
        } else if !arg.starts_with('-') {
            subcommand_at = Some(index);
            break;
        }
    }
    let Some(at) = subcommand_at else {
        return args.iter().any(|arg| matches!(*arg, "--version" | "-V"));
    };
    let Some(subcommand) = args.get(at).copied() else {
        return false;
    };
    let rest: Vec<&str> = args.iter().skip(at).skip(1).copied().collect();
    match subcommand {
        "log" | "status" | "diff" | "show" | "blame" | "rev-parse" | "ls-files" | "ls-tree"
        | "describe" | "shortlog" | "whatchanged" | "cat-file" | "version" | "check-ignore"
        | "check-attr" | "check-mailmap" => true,
        "reflog" => rest
            .first()
            .is_none_or(|sub| matches!(*sub, "show" | "exists")),
        "grep" => !rest
            .iter()
            .any(|arg| arg.starts_with("-O") || arg.starts_with("--open-files-in-pager")),
        "branch" => rest.iter().all(|arg| {
            matches!(
                *arg,
                "-a" | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "-l"
                    | "--list"
                    | "--show-current"
            )
        }),
        "tag" => rest
            .iter()
            .all(|arg| matches!(*arg, "-l" | "-n" | "--list")),
        "remote" => rest.iter().all(|arg| matches!(*arg, "-v" | "--verbose")),
        "stash" => matches!(rest.first().copied(), Some("list" | "show")),
        "worktree" => matches!(rest.first().copied(), Some("list")),
        _ => false,
    }
}

/// The `journalctl` policy: reads, minus the family that clears the
/// journal.
///
/// `--vacuum*`, `--rotate`, `--flush`, `--setup-keys`,
/// `--relinquish-var`, and `--cursor-file` (which plants or
/// overwrites a cursor file) are the writing half of journalctl and
/// refuse
/// anywhere in the command. Everything else reads, and a masked
/// argument is accepted only in a value slot — directly after a flag
/// that takes one (`--since '-6 min'`, `-u dch`) — so a quoted flag
/// cannot hide inside [`QUOTED_ARG`].
fn journalctl_arguments_read_only(args: &[&str]) -> bool {
    let mut previous_took_value = false;
    for arg in args {
        if arg.starts_with("--vacuum")
            || arg.starts_with("--cursor-file")
            || abbreviates(arg, "vacuum-time")
            || abbreviates(arg, "vacuum-size")
            || abbreviates(arg, "cursor-file")
            || matches!(
                *arg,
                "--rotate" | "--flush" | "--setup-keys" | "--relinquish-var"
            )
        {
            return false;
        }
        if *arg == QUOTED_ARG && !previous_took_value {
            return false;
        }
        previous_took_value = matches!(
            *arg,
            "--since" | "--until" | "-u" | "--unit" | "-t" | "--identifier" | "-g" | "--grep"
        );
    }
    true
}

/// The `uniq` policy: one file argument, because the second is an
/// output.
///
/// GNU uniq's positional arguments are `[INPUT [OUTPUT]]` — the
/// second names a file the command *writes* (`uniq in.txt out.txt`
/// creates `out.txt`), so a command naming two positionals is a
/// writer and refuses, exactly as a redirect would. The value-taking
/// width flags (`-f`/`--skip-fields`, `-s`/`--skip-chars`,
/// `-w`/`--check-chars`) consume their following word so a width
/// never counts as a file; every other flag counts or compares and
/// reads. A bare `-` is stdin's filename and counts as a file, so
/// `uniq - out` still refuses — the second positional is real
/// wherever the first came from.
fn uniq_arguments_read_only(args: &[&str]) -> bool {
    let mut takes_value = false;
    let mut operands_only = false;
    let mut files = 0_usize;
    for arg in args {
        if operands_only {
            files = files.saturating_add(1);
        } else if takes_value {
            takes_value = false;
        } else if *arg == "--" {
            operands_only = true;
        } else if matches!(
            *arg,
            "-f" | "-s" | "-w" | "--skip-fields" | "--skip-chars" | "--check-chars"
        ) {
            takes_value = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
        } else {
            files = files.saturating_add(1);
        }
    }
    files <= 1
}

/// The `xxd` policy: one file argument, because the second is an
/// output.
///
/// xxd's positional arguments are `[INFILE [OUTFILE]]` — the second
/// names a file the command *writes* (`xxd in.bin out.hex` creates
/// `out.hex`, over whatever stood there), so a command naming two
/// positionals is a writer and refuses, exactly as `uniq`'s output
/// operand does. The value-taking flags (`-c`/`--cols`,
/// `-g`/`--groupsize`, `-l`/`--len`, `-s`/`--seek`, `-n`/`--name`,
/// `-R`) consume their following word so a bound never counts as a
/// file; every other flag formats or transforms and reads. Option
/// parsing stops at the first operand, as xxd's own argv walk does:
/// a dash-word after the first file is not a flag but the *output*
/// operand (`xxd in.bin -c` opens `-c` for writing and truncates an
/// existing file of that name), so once one file has landed every
/// remaining argument counts as one — the same law `--` always set.
fn xxd_arguments_read_only(args: &[&str]) -> bool {
    let mut takes_value = false;
    let mut operands_only = false;
    let mut files = 0_usize;
    for arg in args {
        if operands_only {
            files = files.saturating_add(1);
        } else if takes_value {
            takes_value = false;
        } else if *arg == "--" {
            operands_only = true;
        } else if matches!(
            *arg,
            "-c" | "--cols"
                | "-g"
                | "--groupsize"
                | "-l"
                | "--len"
                | "-s"
                | "--seek"
                | "-n"
                | "--name"
                | "-R"
        ) {
            takes_value = true;
        } else if arg.starts_with('-') && arg.len() > 1 {
        } else {
            files = files.saturating_add(1);
            if files == 1 {
                operands_only = true;
            }
        }
    }
    files <= 1
}

/// The `find` policy: no mutating primary, and a masked argument only
/// where `find` expects a pattern to compare — never a bare primary.
///
/// `find`'s writing primaries (`-delete`, `-exec`, …) act as
/// standalone words, so a quoted one (`'-delete'`) is still that word
/// and refuses; but the pattern primaries (`-name`, `-path`, …) take a
/// value argument, and a quoted value is the common case (`-name
/// '*.rs'`). A masked token is therefore accepted only in that value
/// position — the argument slot directly after a pattern primary — and
/// refused everywhere else.
fn find_arguments_read_only(args: &[&str]) -> bool {
    let mut previous_took_pattern = false;
    for arg in args {
        if matches!(
            *arg,
            "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls"
        ) || arg.starts_with("-fprint")
        {
            return false;
        }
        if *arg == QUOTED_ARG && !previous_took_pattern {
            return false;
        }
        previous_took_pattern = matches!(
            *arg,
            "-name" | "-iname" | "-path" | "-ipath" | "-regex" | "-iregex"
        );
    }
    true
}

/// Whether a short-flag cluster carries a letter — `sort -ro` is
/// `-r -o`, and the `o` is the output flag.
///
/// Long flags (`--anything`) never combine, so they are excluded
/// here and matched by name instead; only the single-dash cluster is
/// scanned for the letter.
fn short_cluster(arg: &str, letter: char) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains(letter)
}

/// Whether a long option abbreviates `name` — the GNU getopt bargain
/// readers honor.
///
/// A long option may spell any unambiguous prefix of its name, so
/// `--out=x` is `--output=x` and `--se=x` is `--set=x` to the program
/// that receives them; exact-name matching would wave the abbreviation
/// through as an unknown flag. The check errs conservative on purpose:
/// it refuses every prefix of `name`, ambiguous-or-not, because a
/// refused read costs a prompt while a waved-through write costs the
/// file. `less` accepts its long options case-insensitively, so the
/// head is lowered before the comparison.
fn abbreviates(arg: &str, name: &str) -> bool {
    let Some(rest) = arg.strip_prefix("--") else {
        return false;
    };
    let head = rest.split('=').next().unwrap_or("").to_ascii_lowercase();
    !head.is_empty() && name.starts_with(&head)
}

/// One command segment: a single command's text plus the vocabulary
/// the policy reads from it.
///
/// `text` is the exact normalized spelling an exact-text rule
/// remembers and matches; `prefix` is the arity-trimmed verb a
/// prefix rule remembers (empty for a segment too complex to prefix
/// safely); `complex` marks substitution, redirection, shell control
/// flow, or a quoted first word, which are remembered as exact text
/// only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSegment {
    /// The segment's scrubbed, whitespace-normalized text — the
    /// exact spelling an exact-text rule remembers and matches.
    pub text: String,

    /// The arity-trimmed verb prefix — empty for a complex segment.
    ///
    /// The leading verb words with their flags trimmed off —
    /// `cargo build` from `cargo build --workspace` — the shape a
    /// verb-prefix remember names. A complex segment carries none:
    /// no prefix can name what it will run.
    pub prefix: String,

    /// Whether the segment carries substitution, redirection, shell
    /// control flow, or a quoted span in its verb — the first word or
    /// any word inside the arity trim — and is therefore
    /// remembered as exact text only.
    pub complex: bool,
}

/// The shell keywords that make a segment control flow, remembered as
/// exact text only.
///
/// A multi-line loop or conditional splits into fragments at the
/// statement separators; the fragments whose first word is one of
/// these keywords are the control flow itself, which no verb prefix
/// can safely summarize.
const CONTROL_FLOW: &[&str] = &[
    "for", "while", "until", "if", "then", "else", "elif", "fi", "do", "done", "case", "esac",
    "select", "function", "time", "[[",
];

/// Collapse whitespace runs and trim outside quotes; inside quotes,
/// whitespace is data and stays byte-for-byte.
///
/// The normalization an exact command-text rule and the call's
/// segments both apply, so matching is whitespace-insensitive on the
/// words and consistent on both sides — while a quoted payload keeps
/// its own spelling exactly: a newline inside quotes is a different
/// command from a space inside quotes (a `bash -c` payload embedding
/// a newline runs a second line; a space does not), and the exact
/// text an approval remembered for one must never match the other.
/// The quote walk mirrors [`split_commands`]: single-quoted bytes
/// literal, double-quoted escapes consumed in pairs, and a backslash
/// outside quotes gluing its escaped character into the word.
fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                out.push(ch);
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if ch == '"' {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    quote = Some(ch);
                    out.push(ch);
                }
                '\\' => {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push(ch);
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                c if c.is_whitespace() => pending_space = true,
                c => {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push(c);
                }
            },
        }
    }
    out
}

/// Split a scrubbed command into command texts at top-level
/// separators (newline, `;`, `|`, `&`), quote-aware: separators
/// inside single or double quotes, and escaped characters, never
/// split. `&&` and `||` collapse to one break.
fn split_commands(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut iter = command.chars();
    while let Some(ch) = iter.next() {
        match ch {
            '\'' => {
                cur.push(ch);
                for inner in iter.by_ref() {
                    cur.push(inner);
                    if inner == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                cur.push(ch);
                let mut escaped_next = false;
                for inner in iter.by_ref() {
                    if escaped_next {
                        cur.push(inner);
                        escaped_next = false;
                    } else {
                        cur.push(inner);
                        match inner {
                            '"' => break,
                            '\\' => escaped_next = true,
                            _ => {}
                        }
                    }
                }
            }
            '\\' => {
                cur.push(ch);
                if let Some(escaped) = iter.next() {
                    cur.push(escaped);
                }
            }
            '\n' | ';' | '|' | '&' => {
                if cur.trim().is_empty() && ch != '\n' {
                    // the second half of && / || — one break already fired
                    continue;
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .map(|s| normalize_ws(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether the masked segment carries substitution, redirection, or
/// shell control flow — remembered as exact text only.
///
/// A `$`, backtick, or angle bracket surviving the mask is a live
/// substitution or redirection; a parenthesis surviving unescaped is
/// a substitution or grouping the gate cannot summarize; a leading
/// control-flow keyword is the construct itself. All are remembered
/// as exact text, never as a prefix.
fn segment_complex(masked: &str) -> bool {
    if masked.chars().any(|ch| "><`()\\".contains(ch)) || substitution_executes(masked) {
        return true;
    }
    let first = masked.split_whitespace().next().unwrap_or("");
    CONTROL_FLOW.contains(&first)
}

/// Whether the segment's first word is, or contains, a quoted span.
///
/// The first word is the verb — the one thing a prefix rule names —
/// and masking collapses a quoted span to [`QUOTED_ARG`], which names
/// no program. A segment that opens this way is therefore complex:
/// remembered as its exact text, like a substitution, so one
/// approval cannot bless every future command that happens to open
/// with a quote.
fn quoted_verb(masked: &str) -> bool {
    masked
        .split_whitespace()
        .next()
        .is_some_and(|first| first.contains(QUOTED_ARG))
}

/// Reduce a Bash command to its command segments: every command text
/// across newlines and control operators, with its verb prefix and
/// complexity verdict. Readers are segments like any other — they ride
/// the split and the shape like every writer — but the remember mint
/// drops them on both sides of the sheet; the carve-out runs them free
/// on their own. A quoted span inside the trimmed verb masks to the
/// placeholder, which discriminates nothing (`cargo 'test'` and
/// `cargo 'publish'` trim to the same masked verb), so such a segment
/// is complex and remembered as its exact text — the quoted spelling
/// is the only name that tells the two apart.
#[must_use]
pub fn command_segments(command: &str) -> Vec<CommandSegment> {
    let mut out = Vec::new();
    for raw in split_commands(&scrub(command)) {
        let text = normalize_ws(&raw);
        if text.is_empty() {
            continue;
        }
        let masked = mask_quoted(&text);
        let prefix = segment_prefix(&masked);
        let complex =
            segment_complex(&masked) || quoted_verb(&masked) || prefix.contains(QUOTED_ARG);
        out.push(CommandSegment {
            text,
            prefix: if complex { String::new() } else { prefix },
            complex,
        });
    }
    out
}

/// The arity-trimmed verb prefix of one segment.
///
/// The verb table ([`VERB_ARITY`]) supplies the depth — `cargo test`
/// takes two tokens, `npm run dev` three — and the deepest named
/// verb wins, so `cargo run --bin x` trims to `cargo run --bin`. A
/// program the table does not name names itself alone: an unknown
/// command's approval covers exactly its command word, nothing
/// wider.
fn segment_prefix(segment: &str) -> String {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let joined = tokens.join(" ");
    let mut best: Option<(usize, usize)> = None;
    for (verb, arity) in VERB_ARITY {
        let names = joined == *verb || joined.starts_with(&format!("{verb} "));
        if names {
            let depth = verb.split(' ').count();
            if best.is_none_or(|(seen, _)| depth > seen) {
                best = Some((depth, *arity));
            }
        }
    }
    match best {
        Some((_, arity)) => tokens
            .iter()
            .take(arity)
            .copied()
            .collect::<Vec<&str>>()
            .join(" "),
        None => tokens.first().copied().unwrap_or_default().to_string(),
    }
}

/// Whether a call is the session's own furniture — the conversation
/// touching itself.
///
/// The todo list and the question tool note and ask; the jobs table's
/// reads (`jobs`, `job_status`) only look. A note, a question, and a
/// status look touch the conversation, not the world, so they run free
/// in every mode, `ReadOnly` included — while the tools that reach the
/// session's outsides (`Submit`, the jobs table's mutating
/// `cleanup_jobs`) stay matrix-governed. The verdict is name- and
/// operation-based, so a stranger borrowing a furniture name falls
/// through to the fail-closed category as ever.
#[must_use]
pub fn is_session_furniture(tool_name: &str, input: &Value) -> bool {
    if matches!(tool_name, "TodoWrite" | "AskUserQuestion") {
        return true;
    }
    tool_name == "Jobs"
        && input
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|operation| matches!(operation, "jobs" | "job_status"))
}

/// The file paths a write-family call would overwrite, in call order.
///
/// Raw as the call carried them: `Write` and `Edit` name one target at
/// the top level (canonical spelling first, the `path` alias when the
/// canonical is absent entirely), and a `MultiEdit` batch carries one per
/// entry, falling onto the top-level default for pathless entries. Any
/// other tool carries no targets. The write-target gate and the
/// permission gate both read through this one derivation, so the two
/// never disagree about what a call would touch.
#[must_use]
pub fn write_family_targets(tool_name: &str, input: &Value) -> Vec<String> {
    const WRITE_TOOL: &str = "Write";
    const EDIT_TOOL: &str = "Edit";
    const MULTI_EDIT_TOOL: &str = "MultiEdit";
    const FILE_PATH_FIELD: &str = "file_path";
    const PATH_FIELD: &str = "path";
    const EDITS_FIELD: &str = "edits";

    match tool_name {
        WRITE_TOOL | EDIT_TOOL => write_family_path_spelled(input, FILE_PATH_FIELD, PATH_FIELD)
            .map(str::to_string)
            .into_iter()
            .collect(),
        MULTI_EDIT_TOOL => {
            let batch_default = input
                .get(FILE_PATH_FIELD)
                .and_then(Value::as_str)
                .map(str::to_string);
            input
                .get(EDITS_FIELD)
                .and_then(|value| value.as_array())
                .map_or_default(|edits| {
                    edits
                        .iter()
                        .filter_map(|edit| edit_target(edit, batch_default.as_deref()))
                        .collect()
                })
        }
        _ => Vec::new(),
    }
}

/// Resolve the path field from a write-family input, canonical
/// spelling first.
///
/// The fs family's alias tolerance, read the same way the family itself
/// reads it — `file_path` canonical, `path` the drift alias — so the
/// overwrite guard and the remember derivation name one destination.
/// The fs family reads: `file_path` is the canonical key the schemas advertise,
/// `path` is the spelling the recorded model drift sends, and the
/// canonical wins when both ride. A present-but-mistyped canonical
/// resolves to nothing — the tool refuses it with its own teaching
/// error, and a guard must not let a misspelled key slip the
/// destination past it on the alias.
#[must_use]
pub fn write_family_path(input: &Value) -> Option<&str> {
    write_family_path_spelled(input, "file_path", "path")
}

/// The parameterized form the public wrapper delegates to.
///
/// The key spellings ride in as arguments because the batch walk
/// calls it per entry with the same canonical/alias pair the
/// top-level read uses — one logic, two call shapes.
fn write_family_path_spelled<'a>(
    input: &'a Value,
    canonical: &str,
    alias: &str,
) -> Option<&'a str> {
    match input.get(canonical) {
        Some(Value::String(path)) => Some(path.as_str()),
        Some(_) => None,
        None => input.get(alias).and_then(Value::as_str),
    }
}

/// The file one `MultiEdit` entry would overwrite, default leg included.
///
/// The batch resolves an entry with no path of its own onto the
/// top-level `file_path`, so the derivation mirrors the tool's
/// precedence exactly: the entry's own path when it names one (canonical
/// first, alias when the canonical is absent entirely), the batch
/// default when the entry names nothing at all, and nothing when the
/// entry mistypes a path of its own — a call the tool refuses on the
/// entry itself, before the default is ever consulted, so the default is
/// no target that batch reaches.
fn edit_target(edit: &Value, batch_default: Option<&str>) -> Option<String> {
    match write_family_path_spelled(edit, "file_path", "path") {
        Some(path) => Some(path.to_string()),
        None => edit
            .get("file_path")
            .is_none()
            .then(|| {
                edit.get("path")
                    .is_none()
                    .then(|| batch_default.map(str::to_string))
                    .flatten()
            })
            .flatten(),
    }
}

/// The remember-rules for one dispatch: what an "allow always"
/// answer on this ask would record.
///
/// A Bash ask remembers one rule per *writer* segment — verb
/// prefixes for clean segments, exact text for complex ones
/// ([`RememberRules::Segments`]) — and nothing for a reader segment:
/// the carve-out would run a reader free on its own, in every mode,
/// so a rule for it records nothing the gate was asked about, and a
/// chain's differently-worded `echo` tail could otherwise re-ask a
/// chain whose every writer is remembered. A write-family ask
/// remembers the scopes its targets live under
/// ([`RememberRules::PathGlobs`]); every other tool remembers itself
/// ([`RememberRules::Tool`]).
#[must_use]
pub fn remember_rules(tool_name: &str, input: &Value, cwd: &Path) -> RememberRules {
    if tool_name == "Bash" {
        let segments = input
            .get("command")
            .and_then(Value::as_str)
            .map_or_else(Vec::new, command_segments);
        let mut prefixes = Vec::new();
        let mut texts = Vec::new();
        for segment in segments {
            if segment_reads_only(&segment.text) {
                continue;
            }
            if segment.complex {
                texts.push(segment.text);
            } else {
                prefixes.push(segment.prefix);
            }
        }
        return RememberRules::Segments { prefixes, texts };
    }
    let targets = write_family_targets(tool_name, input);
    if !targets.is_empty() {
        return RememberRules::PathGlobs(write_scope_patterns(&targets, cwd));
    }
    RememberRules::Tool(tool_name.to_string())
}

/// The remember-rules for one dispatch: what a "deny always" answer
/// records — narrowly, only what the user plausibly refused.
///
/// The deny-side twin of [`remember_rules`], minting on the
/// same narrow terms. For a Bash ask, reader segments mint nothing — a reader
/// is never what was refused, so refusing `cargo build | tail`
/// bans the build and not the pipe beside it; complex non-reader
/// segments mint their exact text (the shipped
/// [`RememberRules::Segments`] text arm — the redirect or
/// substitution the refusal named, and nothing wider); clean
/// non-reader segments mint the verb prefix. Reader-ness is judged
/// on each segment's whole text by [`segment_reads_only`] — a
/// redirect, substitution, or any live control anywhere in the
/// segment disqualifies it exactly as the whole-command gate would,
/// so a reader verb carrying `>` is a writer here too — and the
/// reader check runs first, so a genuine reader mints nothing. Only
/// a segment the reader gate itself would wave through goes
/// unremembered: the refusal answered once, honestly, and minted
/// nothing by association.
#[must_use]
pub fn deny_rules(tool_name: &str, input: &Value, cwd: &Path) -> RememberRules {
    if tool_name != "Bash" {
        return remember_rules(tool_name, input, cwd);
    }
    let segments = input
        .get("command")
        .and_then(Value::as_str)
        .map_or_else(Vec::new, command_segments);
    let mut prefixes = Vec::new();
    let mut texts = Vec::new();
    for segment in &segments {
        if segment_reads_only(&segment.text) {
            continue;
        }
        if segment.complex {
            texts.push(segment.text.clone());
        } else {
            prefixes.push(segment.prefix.clone());
        }
    }
    RememberRules::Segments { prefixes, texts }
}

/// One remember-glob per distinct scope the write targets live under.
///
/// Takes targets already spelled relative to the working directory —
/// what [`relative_target`] produces — and remembers each one's
/// directory subtree, with the root-level and outside-the-workdir
/// exactness rules the private write-scope helper applies to raw
/// targets.
#[must_use]
pub fn scope_patterns_for_relative(targets: &[String]) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    for target in targets {
        let pattern = match target.strip_prefix('/') {
            // An absolute path — the target lives outside the working
            // directory. Remember the exact file, nothing wider.
            Some(_) => escape_glob_literal(target),
            None => match Path::new(target).parent() {
                // `Path::parent` answers an empty path for a bare
                // filename — the workdir root — where the subtree glob
                // would be the whole workspace, so the file itself is
                // the scope.
                Some(dir) if !dir.as_os_str().is_empty() => {
                    format!("{}/{}", escape_glob_literal(&dir.to_string_lossy()), "**")
                }
                _ => escape_glob_literal(target),
            },
        };
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

/// One remember-glob per distinct scope the raw write targets live
/// under.
///
/// Relativizes first ([`relative_target`]), then applies the scope
/// rules over the relative spelling.
fn write_scope_patterns(targets: &[String], cwd: &Path) -> Vec<String> {
    let relative: Vec<String> = targets
        .iter()
        .map(|target| relative_target(target, cwd))
        .collect();
    scope_patterns_for_relative(&relative)
}

/// Spell `raw` relative to `cwd`, lexically normalized.
///
/// An absolute target is taken as-is; a relative one joins the cwd. `.`
/// and `..` resolve by walking components — no filesystem call, so a
/// target that does not exist yet relativizes exactly like one that
/// does. A path that climbs above the cwd keeps its normalized absolute
/// spelling, which no workspace-scoped glob matches: the safe fall-through
/// is that such a target asks again.
#[must_use]
pub fn relative_target(raw: &str, cwd: &Path) -> String {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize_lexical(&absolute);
    let cwd = normalize_lexical(cwd);
    match normalized.strip_prefix(&cwd) {
        Ok(rest) if !rest.as_os_str().is_empty() => rest.to_string_lossy().into_owned(),
        _ => normalized.to_string_lossy().into_owned(),
    }
}

/// Resolve `.` and `..` components by walking, touching no filesystem.
///
/// A path that climbs above the root stops there — `pop` on an empty
/// buffer is a no-op — and a relative path comes back relative. Purely
/// lexical, so a target that does not exist yet relativizes exactly
/// like one that does, and the write-target gate's metadata check stays
/// the only call that reads the disk.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Escape glob metacharacters so a literal name matches only itself.
///
/// Each character class metacharacter (`*`, `?`, brackets, braces) is
/// wrapped in a character class of itself — globset's spelling for a
/// literal — and a backslash is doubled, since it is globset's escape
/// character. A directory named `my[dir]` produces `my[[]dir[]]` in
/// pattern spelling: matched literally, never as a class.
fn escape_glob_literal(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len());
    for ch in literal.chars() {
        match ch {
            '*' | '?' | '[' | ']' | '{' | '}' => {
                escaped.push('[');
                escaped.push(ch);
                escaped.push(']');
            }
            '\\' => {
                escaped.push('\\');
                escaped.push('\\');
            }
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::PermissionMode;
    use crate::vocabulary::PermissionOutcome;
    use crate::vocabulary::ToolCategory;

    #[test]
    fn remember_rules_records_every_writer_segment_of_the_chain() {
        // One arity-trimmed verb prefix per *writer* segment — readers
        // mint nothing, on either side of the sheet: the carve-out
        // would run them free on their own, so a rule for them records
        // nothing the approval was about, and a differently-worded
        // `echo` tail must never re-ask a chain whose writers are all
        // remembered. The inert `2>&1` is scrubbed before splitting, so
        // it never becomes a segment of its own.
        let rules = |command: &str| {
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            rules("cargo build --workspace 2>&1 | tail -15"),
            RememberRules::Segments {
                prefixes: vec!["cargo build".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            rules("cargo test -p dch-tui --all-features | grep -E 'x|y' | head -3"),
            RememberRules::Segments {
                prefixes: vec!["cargo test".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            rules("npm run dev && cargo build"),
            RememberRules::Segments {
                prefixes: vec!["npm run dev".to_string(), "cargo build".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            rules("cargo run --bin dch"),
            RememberRules::Segments {
                prefixes: vec!["cargo run --bin".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            rules("perl -0pi -e 's/x/y/' file"),
            RememberRules::Segments {
                prefixes: vec!["perl".to_string()],
                texts: Vec::new(),
            }
        );
        // A multi-line script is remembered per line — opencode parity.
        assert_eq!(
            rules("cargo build\nrm -rf /"),
            RememberRules::Segments {
                prefixes: vec!["cargo build".to_string(), "rm".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            remember_rules("WebFetch", &serde_json::json!({}), Path::new("/work")),
            RememberRules::Tool("WebFetch".to_string())
        );
    }

    #[test]
    fn a_quoted_first_word_is_complex_and_mints_no_verb_prefix() {
        // The first word of a segment is its verb; quoted, masking
        // collapses it to the placeholder, which names no program. A
        // prefix rule built on it would bless every future
        // quoted-first-word command, so the segment is complex instead:
        // remembered as its exact text, like a substitution.
        let segments = command_segments("'cat' notes.txt");
        let first = segments.first().unwrap();
        assert!(first.complex, "a quoted verb cannot be named by a prefix");
        assert_eq!(first.prefix, "", "a complex segment carries no prefix");
        assert_eq!(first.text, "'cat' notes.txt");
        assert_eq!(
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": "'cat' notes.txt" }),
                Path::new("/work")
            ),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["'cat' notes.txt".to_string()],
            }
        );
    }

    #[test]
    fn a_partial_quote_on_the_first_word_is_complex_too() {
        // Concatenation spelling: `'cat'x` is one word whose masking
        // carries the placeholder beside real text — still no nameable
        // verb, still complex, so the approval cannot widen past the
        // exact spelling.
        let segments = command_segments("'cat'x notes.txt");
        let first = segments.first().unwrap();
        assert!(first.complex, "a placeholder inside the verb names nothing");
        assert_eq!(first.prefix, "");
    }

    #[test]
    fn mid_word_inert_spellings_keep_their_redirect() {
        // The scrub is a token bargain, not a substring erase: inside a
        // word the spelling is text, not a redirect, and the `>` it
        // carries must keep the command out of the reader gate. Both
        // glued edges pin — the splice behind a word and the tail
        // running past /dev/null — and the standalone forms stay
        // scrubbed (pinned in the reader list above).
        assert!(
            !shell_reads_only("echo x2>/dev/nullbar"),
            "x2>/dev/nullbar redirects to /dev/nullbar — a writer"
        );
        assert!(
            !shell_reads_only("cat /tmp/2>/dev/null.txt"),
            "the path word ends in the spelling — still a real redirect"
        );
        assert!(
            !shell_reads_only("cat 2>/dev/null.txt"),
            "a target that runs past /dev/null is a real file"
        );
        assert!(
            shell_reads_only("grep '2>/dev/null' notes.txt"),
            "quoted, the spelling is literal text — nothing redirects"
        );
        let segments = command_segments("cargo test2>/dev/nullx");
        let first = segments.first().unwrap();
        assert!(
            first.complex,
            "the glued redirect keeps the segment complex"
        );
    }

    #[test]
    fn deny_rules_mint_only_what_was_refused() {
        // The narrow derivation, pinned end to end: the deny list
        // gains only `cargo build` —
        // `tail`, a pure reader, is not banned by association. The
        // inert `2>&1` is scrubbed before splitting, exactly as the
        // allow derivation scrubs it, so the redirect never widens the
        // mint.
        let deny = |command: &str| {
            deny_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            deny("cargo build --workspace --quiet 2>&1 | tail -30"),
            RememberRules::Segments {
                prefixes: vec!["cargo build".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            deny("make ci 2>&1 | grep -E 'fail|error' | head -20"),
            RememberRules::Segments {
                prefixes: vec!["make ci".to_string()],
                texts: Vec::new(),
            }
        );
    }

    #[test]
    fn both_remember_sides_drop_readers_and_keep_writers() {
        // The symmetry both sides now keep: the same chain's approval
        // and its refusal remember the writers alone — a reader is
        // never what the answer was about, so it mints nothing on
        // either side, and the file never grows an `echo`/`head`/
        // `tail` row per wording.
        let input = serde_json::json!({
            "command": "cargo build --workspace --quiet 2>&1 | tail -30"
        });
        assert_eq!(
            remember_rules("Bash", &input, Path::new("/work")),
            RememberRules::Segments {
                prefixes: vec!["cargo build".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            deny_rules("Bash", &input, Path::new("/work")),
            RememberRules::Segments {
                prefixes: vec!["cargo build".to_string()],
                texts: Vec::new(),
            }
        );
    }

    #[test]
    fn a_complex_non_reader_segment_mints_exact_text_into_deny() {
        // A redirect to a real file makes the segment complex: the
        // refusal mints that exact line and nothing wider — a verb
        // prefix minted from a payload-carrying segment could outrun
        // what the user saw. A reader verb carrying the redirect is a
        // writer the same way (`cat notes.txt > owned.txt`), and a
        // substitution disqualifies its segment exactly as the
        // whole-command gate would — both mint their exact text, so
        // the denial stands against the identical rerun after a mode
        // move.
        let deny = |command: &str| {
            deny_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            deny("make test > build.log | tail -5"),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["make test > build.log".to_string()],
            }
        );
        assert_eq!(
            deny("cat notes.txt > owned.txt"),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["cat notes.txt > owned.txt".to_string()],
            }
        );
        assert_eq!(
            deny("grep \"$(cat patterns)\" notes.md | sort | head -5"),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["grep \"$(cat patterns)\" notes.md".to_string()],
            }
        );
    }

    #[test]
    fn a_uniq_second_positional_names_an_output_file() {
        // GNU uniq writes its second positional: one file argument
        // keeps it a look (an input, or stdin to stdout), two make it
        // a writer the reader gate refuses — counting flags and their
        // attached values out, and a consumed width value out too.
        assert!(
            shell_reads_only("uniq"),
            "bare uniq reads stdin and writes stdout"
        );
        assert!(
            shell_reads_only("uniq -c input.txt"),
            "one positional is an input — still a look"
        );
        assert!(
            shell_reads_only("uniq -f 2 -s 1 input.txt"),
            "width values are consumed, not files"
        );
        assert!(
            !shell_reads_only("uniq input.txt output.txt"),
            "the second positional is an output file — a writer"
        );
        assert!(
            !shell_reads_only("uniq -c input.txt output.txt"),
            "flags do not hide the second positional"
        );
        assert!(
            !shell_reads_only("uniq -f 2 input.txt output.txt"),
            "a consumed width value does not hide it either"
        );
        assert!(
            !shell_reads_only("uniq - out.txt"),
            "a bare dash is stdin's name — out.txt is still second"
        );
        assert!(
            !shell_reads_only("uniq -- -f out.txt"),
            "past `--` a flag spelling is an operand: -f is the input, out.txt \
             the output GNU uniq writes — a writer, not a look"
        );
        assert!(
            !shell_reads_only("uniq -- -f2 out.txt"),
            "the attached-value spelling is an operand past `--` too — the \
             second word still names the output"
        );
        assert!(
            shell_reads_only("uniq -- input.txt"),
            "a lone operand past `--` is an input like any other — a look"
        );
    }

    #[test]
    fn quoted_payload_whitespace_is_data_not_a_separator() {
        // A newline inside quotes and a space inside quotes are
        // different commands — the newline executes a second line
        // under `bash -c`, the space does not — so the exact text an
        // approval remembers keeps quoted whitespace byte-for-byte
        // and the two never cross-match, in either direction.
        let newline = "'bash' -c 'echo safe\nrm victim'";
        let spaces = "'bash' -c 'echo safe rm victim'";
        let newline_segments = command_segments(newline);
        assert_eq!(
            newline_segments.len(),
            1,
            "a quoted newline does not split the segment"
        );
        assert_eq!(
            newline_segments.first().map(|s| s.text.as_str()),
            Some(newline),
            "the quoted newline survives normalization as data"
        );
        assert_ne!(
            newline_segments.first().map(|s| s.text.as_str()),
            command_segments(spaces).first().map(|s| s.text.as_str()),
            "the newline twin and the space twin are different commands"
        );
        let shape_for = |cmd: &str| crate::engine::CallShape {
            segments: command_segments(cmd),
            ..crate::engine::CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let remembered = crate::engine::RememberedRules {
            command_texts: vec![newline.to_string()],
            ..crate::engine::RememberedRules::empty()
        };
        assert!(
            remembered.covers(&shape_for(newline)),
            "the identical rerun runs free through its exact text"
        );
        assert!(
            !remembered.covers(&shape_for(spaces)),
            "the space twin re-asks — quoted whitespace is data"
        );
    }

    #[test]
    fn a_quoted_word_inside_the_trimmed_verb_mints_no_prefix() {
        // `cargo 'test'` and `cargo 'publish'` mask to the same
        // trimmed verb — `cargo` plus the placeholder — so a prefix
        // minted from either would cover both. The segment goes
        // complex instead: remembered as its exact text, the quoted
        // spelling being the one name that tells them apart.
        let segments = command_segments("cargo 'test'");
        let first = segments.first().unwrap();
        assert!(
            first.complex,
            "a quoted word in the verb names nothing wider"
        );
        assert_eq!(first.prefix, "", "a complex segment carries no prefix");
        assert_eq!(first.text, "cargo 'test'");
        assert_eq!(
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": "cargo 'test'" }),
                Path::new("/work")
            ),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["cargo 'test'".to_string()],
            }
        );
        // A quoted argument beyond the trimmed verb is payload, not
        // name: the prefix stays `cargo test` and covers its flags.
        assert_eq!(
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": "cargo test 'foo'" }),
                Path::new("/work")
            ),
            RememberRules::Segments {
                prefixes: vec!["cargo test".to_string()],
                texts: Vec::new(),
            }
        );
        let shape_for = |cmd: &str| crate::engine::CallShape {
            segments: command_segments(cmd),
            ..crate::engine::CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let remembered = crate::engine::RememberedRules {
            command_texts: vec!["cargo 'test'".to_string()],
            ..crate::engine::RememberedRules::empty()
        };
        assert!(
            !remembered.covers(&shape_for("cargo 'publish'")),
            "one quoted verb word cannot cover another — the placeholder discriminates nothing"
        );
        assert!(
            remembered.covers(&shape_for("cargo 'test'")),
            "the identical rerun runs free through its exact text"
        );
    }

    #[test]
    fn an_argument_disqualified_reader_still_mints_its_verb() {
        // Reader-ness is judged on the segment's whole text, so the
        // verb's arguments decide: a deleting `find` is no reader — its
        // refusal mints the verb, the widest lever the press names —
        // while a searching `find` is one and mints nothing. That
        // sharpness is accepted and made visible on the sheet rather
        // than narrowed by a writer-verb table.
        let deny = |command: &str| {
            deny_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            deny("find . -delete | sort"),
            RememberRules::Segments {
                prefixes: vec!["find".to_string()],
                texts: Vec::new(),
            }
        );
        assert_eq!(
            deny("find . -name x | sort"),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: Vec::new(),
            }
        );
    }

    #[test]
    fn an_all_reader_derivation_records_nothing() {
        // A chain the reader gate would never have asked (every
        // segment reads) derives an empty remember set on the deny
        // side: the refusal answered once, honestly, and minted
        // nothing by association.
        assert_eq!(
            deny_rules(
                "Bash",
                &serde_json::json!({ "command": "grep x notes.md | sort | head -3" }),
                Path::new("/work"),
            ),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: Vec::new(),
            }
        );
    }

    #[test]
    fn write_family_and_tool_denials_mint_what_the_allow_side_mints() {
        // The write scopes and the whole-tool remember are unchanged on
        // the deny side: the refusal that reached them named exactly
        // what they record.
        let write = serde_json::json!({ "file_path": "crates/loop/src/lib.rs" });
        assert_eq!(
            deny_rules("Write", &write, Path::new("/work")),
            remember_rules("Write", &write, Path::new("/work"))
        );
        assert_eq!(
            deny_rules("WebFetch", &serde_json::json!({}), Path::new("/work")),
            RememberRules::Tool("WebFetch".to_string())
        );
    }

    #[test]
    fn complex_writers_are_exact_text_and_variable_readers_mint_nothing() {
        // The two halves of the substitution split. A *bare* variable
        // — `$?` inside double quotes — only expands: the echo that
        // reports an exit code is a looker, minting nothing, so no
        // per-wording row ever lands in the file. An *executing*
        // substitution — `$(…)` inside the quotes — keeps the segment
        // complex and remembers its exact normalized text, never a
        // verb prefix that could ride a payload; the clean cargo
        // segment beside it still mints its verb, and one segment's
        // complexity never contaminates the chain's other segments.
        let rules = |command: &str| {
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            rules("cargo check --workspace --quiet 2>&1 | tail -30; echo \"EXIT: $?\""),
            RememberRules::Segments {
                prefixes: vec!["cargo check".to_string()],
                texts: Vec::new(),
            },
            "the variable-carrying echo is a reader: it mints nothing, and no \
             differently-worded twin of it will ever re-ask the chain"
        );
        assert_eq!(
            rules("cargo check 2>&1; echo \"wrote: $(git rev-parse HEAD)\""),
            RememberRules::Segments {
                prefixes: vec!["cargo check".to_string()],
                texts: vec!["echo \"wrote: $(git rev-parse HEAD)\"".to_string()],
            },
            "an executing substitution inside the quotes stays exact text, \
             never an `echo` verb prefix that could ride a payload"
        );
        assert_eq!(
            rules("cargo build > owned.txt"),
            RememberRules::Segments {
                prefixes: Vec::new(),
                texts: vec!["cargo build > owned.txt".to_string()],
            }
        );
    }

    #[test]
    fn write_asks_remember_directory_scopes() {
        // A file below the root remembers its directory's subtree; a file
        // at the root — where a subtree glob would be the whole workspace
        // — remembers itself; a target outside the working directory
        // remembers its exact path; a batch spanning two directories
        // remembers both scopes, once each.
        let write = |input: Value| remember_rules("Write", &input, Path::new("/work"));
        assert_eq!(
            write(serde_json::json!({ "file_path": "crates/x/src.rs" })),
            RememberRules::PathGlobs(vec!["crates/x/**".to_string()])
        );
        assert_eq!(
            write(serde_json::json!({ "file_path": "README.md" })),
            RememberRules::PathGlobs(vec!["README.md".to_string()])
        );
        assert_eq!(
            write(serde_json::json!({ "file_path": "/outside/x" })),
            RememberRules::PathGlobs(vec!["/outside/x".to_string()])
        );
        assert_eq!(
            write(serde_json::json!({ "file_path": "../escape.txt" })),
            RememberRules::PathGlobs(vec!["/escape.txt".to_string()])
        );
        assert_eq!(
            remember_rules(
                "MultiEdit",
                &serde_json::json!({
                    "file_path": "a/one.rs",
                    "edits": [
                        { "old_text": "x", "new_text": "y" },
                        { "file_path": "b/two.rs", "old_text": "x", "new_text": "y" }
                    ]
                }),
                Path::new("/work"),
            ),
            RememberRules::PathGlobs(vec!["a/**".to_string(), "b/**".to_string()])
        );
        // A directory spelled with glob metacharacters is escaped, so the
        // remember matches the literal directory and never a class.
        assert_eq!(
            write(serde_json::json!({ "file_path": "my[dir]/x" })),
            RememberRules::PathGlobs(vec!["my[[]dir[]]/**".to_string()])
        );
    }

    #[test]
    fn an_inert_set_preamble_mints_nothing_and_demands_no_coverage() {
        // The option setter reads like a look, so a chain carrying an
        // inert `set -o pipefail` preamble passes the reader gate
        // whole — and reads like a look to the remember side too: the
        // preamble mints nothing and demands no coverage, because the
        // chain's one writer is the runner. A remembered writer covers
        // the chain whole; a chain whose writer is not remembered
        // re-asks whatever readers ride beside it. The pin drives both
        // halves, prefix set and coverage.
        let command =
            "set -o pipefail; cargo test --workspace 2>&1 | grep -E 'Running|Doc-tests' | tail -40";
        let segments = command_segments(command);
        let prefixes: Vec<String> = segments
            .iter()
            .map(|segment| segment.prefix.clone())
            .collect();
        assert_eq!(
            prefixes,
            vec![
                "set".to_string(),
                "cargo test".to_string(),
                "grep".to_string(),
                "tail".to_string()
            ],
            "the split carries every command — the inert preamble its own segment"
        );
        let shape_for = |cmd: &str| crate::engine::CallShape {
            segments: command_segments(cmd),
            ..crate::engine::CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let wrong_writer = crate::engine::RememberedRules {
            shell_prefixes: vec!["cargo build".to_string()],
            ..crate::engine::RememberedRules::empty()
        };
        assert!(
            !wrong_writer.covers(&shape_for(command)),
            "a chain whose writer is not remembered re-asks — whatever readers \
             ride beside it"
        );
        let the_writer_alone = crate::engine::RememberedRules {
            shell_prefixes: vec!["cargo test".to_string()],
            ..crate::engine::RememberedRules::empty()
        };
        assert!(
            the_writer_alone.covers(&shape_for(command)),
            "the remembered writer covers the chain whole — the preamble and \
             the reader tail demand nothing"
        );
        assert_eq!(
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work")
            ),
            RememberRules::Segments {
                prefixes: vec!["cargo test".to_string()],
                texts: Vec::new(),
            },
            "the press remembers the runner — the chain's one writer — and \
             nothing for the preamble or the reader tail"
        );
        assert!(
            the_writer_alone.covers(&shape_for(command)),
            "the answer that remembered the chain's writer covers it from then on"
        );
        // Anything `set` can do beyond the known failure-behavior options
        // keeps its own verb — remembered separately when approved.
        let variants = command_segments("set -a; cargo test --workspace");
        let prefixes: Vec<String> = variants
            .iter()
            .map(|segment| segment.prefix.clone())
            .collect();
        assert_eq!(prefixes, vec!["set".to_string(), "cargo test".to_string()],);
    }

    #[test]
    fn a_remembered_writer_covers_a_chain_with_differently_worded_readers() {
        // The report that motivated the split, pinned end to end:
        // approving `cargo fmt` with an exit-code echo beside it mints
        // the writer alone, and the same chain with a differently-
        // worded echo — or a different reader entirely — runs free: no
        // second ask, no second row in the permissions file. A new
        // writer still re-asks.
        let rules = |command: &str| {
            remember_rules(
                "Bash",
                &serde_json::json!({ "command": command }),
                Path::new("/work"),
            )
        };
        assert_eq!(
            rules("cargo fmt && echo \"exit: $?\""),
            RememberRules::Segments {
                prefixes: vec!["cargo fmt".to_string()],
                texts: Vec::new(),
            },
            "the approval mints the writer alone — the echo is a reader"
        );
        let shape_for = |cmd: &str| crate::engine::CallShape {
            segments: command_segments(cmd),
            ..crate::engine::CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let remembered = crate::engine::RememberedRules {
            shell_prefixes: vec!["cargo fmt".to_string()],
            ..crate::engine::RememberedRules::empty()
        };
        assert!(
            remembered.covers(&shape_for("cargo fmt && echo \"fmt-all exit: $?\"")),
            "a differently-worded echo never re-asks the chain"
        );
        assert!(
            remembered.covers(&shape_for("cargo fmt | tail -3")),
            "a different reader beside the writer never re-asks either"
        );
        assert!(
            !remembered.covers(&shape_for("cargo build && echo \"exit: $?\"")),
            "a new writer re-asks — the reader beside it changes nothing"
        );
        assert!(
            !remembered.covers(&shape_for("cargo fmt && xxd input.bin victim.hex")),
            "a reader-named writer — xxd's second operand is its output — \
             re-asks: the carve-out never carried it, on the remembered \
             side or the config's"
        );
    }

    #[test]
    fn a_path_prefixed_program_outside_the_system_bins_is_not_a_reader() {
        // The reader list names programs, not basenames of arbitrary
        // paths: a repository can ship an executable that merely shares
        // a reader's name, so `./cat`, `bin/rg`, and any other relative
        // or off-system absolute path refuse — the safe bias costs a
        // prompt, a waved-through write costs the file. The standard
        // system bin directories keep the basename treatment their
        // absolute spellings always carried.
        assert!(
            !shell_reads_only("./cat notes.txt"),
            "a relative path is not the reader list's cat"
        );
        assert!(
            !shell_reads_only("bin/rg -n TODO"),
            "a repo-local binary is not rg"
        );
        assert!(
            !shell_reads_only("/tmp/evil/cat /etc/shadow"),
            "an absolute path outside the system bins is not trusted"
        );
        assert!(
            shell_reads_only("/usr/bin/git log"),
            "the system bins keep their basename treatment"
        );
        assert!(shell_reads_only("/bin/ls -la"), "so does /bin");
        // Traversal spellings: a `..` component walks out of whatever
        // prefix the spelling carries, and the kernel resolves it at
        // execve — none of these is a system-bin program however it
        // starts.
        assert!(
            !shell_reads_only("/usr/bin/../../tmp/evil/cat /etc/shadow"),
            "a dot-dot component never survives the system-bin prefix"
        );
        assert!(
            !shell_reads_only("/bin/../tmp/evil/rg -n TODO"),
            "one hop out of /bin is out of the system bins"
        );
        assert!(
            !shell_reads_only("/usr/local/bin/../../../home/x/evil/cat secret"),
            "a deep walk out of /usr/local/bin is out of the system bins"
        );
        assert!(
            !shell_reads_only("../evil/cat notes.txt"),
            "a relative traversal refuses like any relative path"
        );
    }

    #[test]
    fn read_only_shell_commands_read_like_file_tools() {
        // The carve-out's gate: a command that only looks qualifies,
        // and a pipeline of readers reads — every segment of the chain
        // must itself be a known reader whose arguments keep it
        // reading. Anything that redirects, substitutes, hides a
        // second line, chains in one writer (`rm`, `sh`, `cargo`), or
        // runs a writing subcommand or flag — git's above all, `find
        // -delete`, `sort -o`, bare `env` followed by a command —
        // does not.
        for command in [
            "git log --oneline",
            "git status",
            "git diff HEAD~1",
            "git show abc123",
            "ls -la",
            "cat README.md",
            "/usr/bin/git log",
            "wc -l Cargo.toml",
            "rg -n TODO | wc -l",
            "git log | wc -l",
            "git grep -n TODO",
            "cat notes.txt | sort | uniq -c",
            "uniq -c input.txt",
            "jq .name package.json",
            "find . -name '*.rs' -print",
            "sort -n list.txt",
            "env | grep PATH",
            "pwd && ls -la",
            "ls -la 2>/dev/null",
            "git log --oneline -15 2>/dev/null",
            "cat notes.txt 2>&1 | head -5",
            "cd ~/source && grep -rn TODO --include='*.rs' | head -20",
            "echo --- && pwd",
            "set -o pipefail",
            "set -o errexit -o nounset",
            "set",
            "git diff HEAD -- crates/dch-loop/src/permission.rs | grep -E '^@@|^\\+pub|^\\+fn' | head -40",
            "echo 'a > b'",
            "echo \"done: $?\"",
            "xxd Cargo.lock",
            "grep 'x|y' notes.txt",
            "head -6 \"README.md\"",
            "git -C /home/dch/repos/dch log --oneline -15",
            "git --no-pager log",
            "git -C /home/dch/repos/dch log --oneline -15 && echo \"---STATUS---\" && git -C /home/dch/repos/dch status --short --branch",
            "git check-ignore -v AGENTS.md run.err run.out",
            "git branch",
            "git branch -a -v",
            "git tag",
            "git remote -v",
            "git stash list",
            "git worktree list",
            "cargo tree -p dch-tools --depth 1",
            "cargo version",
            "pgrep -f 'make ci'",
            "journalctl --user -u dch --since '-6 min' --no-pager | tail -5",
            "systemctl --user is-active dch",
            "git --version",
            "find . \\( -name '*.rs' -o -name '*.txt' \\)",
            "grep foo\\|bar notes.txt",
        ] {
            assert!(shell_reads_only(command), "{command} only looks");
        }
        for command in [
            "git log; rm -rf /",
            "git push",
            "git commit -m x",
            "git log | sh",
            "git log --output=owned.txt",
            "wc --output=owned.txt Cargo.toml",
            "rg --pre=/tmp/x.sh TODO .",
            "rg --pre cat TODO",
            "rg --pager=less TODO",
            // The quoting arms: a wholly quoted argument refuses for
            // rg — flag or pattern, the mask cannot tell them apart —
            // and a splice into a flag's name refuses for every
            // reader, whatever program carries it.
            "rg '--pre=./evil.sh' TODO README.md",
            "rg '--pre' cat TODO README.md",
            "rg '--pre'=./evil.sh TODO README.md",
            "rg --pre='./evil.sh' TODO README.md",
            "rg 'TODO|FIXME' README.md",
            "journalctl --vac'u'um-time=1s",
            "find . '-de'lete",
            "less '-o'=logfile notes.txt",
            // `-O` is the log-file family's uppercase spelling — the
            // unconditional-overwrite one, strictly worse than the
            // `-o` — and either case refuses inside a cluster, so
            // the glued spellings ride no cluster past the arm.
            "less -O owned.txt notes.txt",
            "cat x | less -O /tmp/leak",
            // An unquoted `$` word-splits into whatever it holds — new
            // words and flags alike — so no unquoted expansion reads,
            // while the quoted exit-code echo above still does.
            "echo $HOME $USER",
            "find ./sandbox $ACTION",
            // xxd's second positional is its own output operand — the
            // hex dump's file — so one file argument is all it keeps.
            "xxd input.bin victim.hex",
            "cargo fmt && xxd input.bin victim.hex",
            "cat x | less -O/tmp/leak",
            "cat x | less -XO /tmp/leak",
            "cat x | less -NSO /tmp/leak notes.txt",
            "date '--set'=12:00",
            "git log --'output'=owned.txt",
            "git -c core.pager=cat status",
            "git -c diff.external=x log --ext-diff -p",
            "git grep -Oevil.sh TODO",
            "git grep -O TODO",
            "git grep --open-files-in-pager=evil.sh TODO",
            "git grep --open-files-in-pager less TODO",
            "git --ext-diff log",
            "git --paginate log",
            "systemctl -H user@host is-active dch",
            "systemctl --host=user@host is-active dch",
            "systemctl -M host is-active dch",
            "journalctl -n 1 --cursor-file=/tmp/cursor",
            "journalctl --cursor-file /tmp/cursor",
            "cargo build | wc -l",
            "ls > owned.txt",
            "echo $(rm -rf /)",
            "echo \"x$(rm)y\"",
            "echo \"${IFS}x\"",
            "find . -exec rm {} \\;",
            "find . -delete",
            "find . -execdir rm {} +",
            "sort -o owned.txt list.txt",
            "sort --compress-program=x list.txt",
            "sort --compress-program x list.txt",
            "sort -S 4M --compress-program=x big.txt",
            "gh pr list",
            "gh run view 123",
            "sort -ro owned.txt list.txt",
            "uniq input.txt output.txt",
            "uniq -f 2 input.txt output.txt",
            "date -s '12:00'",
            "env rm -rf /tmp/x",
            "echo hi > owned.txt",
            "cat notes.txt 2>err.txt",
            "echo \"$(rm -rf /)\"",
            "env 'rm -rf /'",
            "sort '-ro' out list",
            "set -a",
            "set -e",
            "set -o allexport",
            "set -- x",
            "set -o pipefail; rm -rf /",
            "bash -c 'true'",
            "find . '-delete'",
            "find . -name '*.rs' '-delete'",
            "git -C /tmp push",
            "git --output=x log",
            "git branch -D main",
            "git tag v1.0",
            "git remote add origin https://example.invalid",
            "git stash pop",
            "git worktree add ../wt",
            "cargo build",
            "cargo +nightly tree",
            "journalctl --vacuum-time=1s",
            "journalctl '--vacuum-time=1s'",
            "systemctl restart dch",
            "gh pr merge 5",
            "git log\nrm -rf /",
            "",
            "cargo test",
        ] {
            assert!(
                !shell_reads_only(command),
                "{command} is a shell command like any other"
            );
        }
    }

    #[test]
    fn long_option_abbreviations_refuse_like_their_full_names() {
        // The GNU getopt bargain: a long option may spell any
        // unambiguous prefix of its name, so `--out=x` IS `--output=x`
        // and `--se=x` IS `--set=x` to the program that parses them.
        // Exact-name matching would wave the abbreviation through as
        // an unknown flag; the reader arms refuse every prefix of
        // their dangerous options, and the shared `--output` refusal
        // carries the same bargain to every reader.
        for command in [
            "sort --out=x list.txt",
            "sort --com=x list.txt",
            "date --se=12:00",
            "less --log=x notes.txt",
            "journalctl --cursor=/tmp/cursor",
            "wc --out=x Cargo.toml",
            "rg --out=x TODO .",
        ] {
            assert!(
                !shell_reads_only(command),
                "{command} abbreviates a writing option and refuses"
            );
        }
        for command in [
            "sort -n list.txt",
            "date --rfc-3339=seconds",
            "journalctl --unit dch --no-pager",
            "wc -l Cargo.toml",
        ] {
            assert!(
                shell_reads_only(command),
                "{command} spells no dangerous option, in full or in prefix"
            );
        }
    }

    #[test]
    fn git_reflog_reads_only_through_its_reading_half() {
        // Bare `git reflog` lists the log, and `show`/`exists` consult
        // it; `expire` and `delete` rewrite the very record the
        // carve-out was trusting, so every other subcommand refuses.
        for command in ["git reflog", "git reflog show", "git reflog exists HEAD"] {
            assert!(shell_reads_only(command), "{command} only looks");
        }
        for command in ["git reflog expire --all", "git reflog delete HEAD@{0}"] {
            assert!(
                !shell_reads_only(command),
                "{command} rewrites the reflog the reader was trusting"
            );
        }
    }

    #[test]
    fn xxd_reads_flags_before_the_first_operand_and_writes_after_it() {
        // O1's law pinned on its own: option parsing stops at the first
        // file operand, exactly as xxd's own argv walk does. Ahead of
        // it, flags and their values read like the one-file form; after
        // it, a dash-word is not a flag but the *output* operand — real
        // xxd opens it for writing and truncates whatever stood there
        // (`xxd input.bin -c` destroys a file named `-c`) — and the
        // two-positional form stays a writer, `--` or not.
        for command in [
            "xxd Cargo.lock",
            "xxd -c 16 Cargo.lock",
            "xxd -l 64 -g 2 Cargo.lock",
        ] {
            assert!(shell_reads_only(command), "{command} only looks");
        }
        for command in [
            "xxd input.bin victim.hex",
            "xxd input.bin -c",
            "xxd input.bin -c 16",
        ] {
            assert!(
                !shell_reads_only(command),
                "{command} writes an output operand after the first file"
            );
        }
    }

    #[test]
    fn the_furniture_verdict_is_name_and_operation() {
        // Todo and question are furniture whatever the mode; the jobs
        // table's reads are, its mutator is not; and everything else is
        // matrix-governed.
        assert!(is_session_furniture("TodoWrite", &serde_json::json!({})));
        assert!(is_session_furniture(
            "AskUserQuestion",
            &serde_json::json!({})
        ));
        assert!(is_session_furniture(
            "Jobs",
            &serde_json::json!({ "operation": "jobs" })
        ));
        assert!(is_session_furniture(
            "Jobs",
            &serde_json::json!({ "operation": "job_status" })
        ));
        assert!(!is_session_furniture(
            "Jobs",
            &serde_json::json!({ "operation": "cleanup_jobs" })
        ));
        assert!(!is_session_furniture("Submit", &serde_json::json!({})));
    }

    #[test]
    fn the_matrix_still_resolves_every_cell_after_the_move() {
        // A sentinel over the moved `decide`: these cells land where
        // they always did, so the relocation changed no verdict. The
        // full 18-cell matrix is
        // `the_mode_category_matrix_resolves_every_cell`'s, in
        // `vocabulary.rs`.
        use PermissionMode::{Auto, Interactive, ReadOnly};
        use PermissionOutcome::{Allow, Ask, Block};
        for (mode, category, expected) in [
            (Auto, ToolCategory::FileRead, Allow),
            (Auto, ToolCategory::ShellExecute, Allow),
            (Auto, ToolCategory::Unclassified, Allow),
            (ReadOnly, ToolCategory::FileRead, Allow),
            (ReadOnly, ToolCategory::FileWrite, Block),
            (ReadOnly, ToolCategory::ShellExecute, Block),
            (Auto, ToolCategory::FileWrite, Allow),
            (Interactive, ToolCategory::ShellExecute, Ask),
            (Interactive, ToolCategory::FileWrite, Ask),
            (Interactive, ToolCategory::ShellExecute, Ask),
        ] {
            assert_eq!(
                crate::vocabulary::decide(mode, category),
                expected,
                "{mode:?} × {category:?}"
            );
        }
    }
}
