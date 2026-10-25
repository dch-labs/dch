//! Guard against whole-file writes over content the redactor hides.
//!
//! While the pipeline redacts, every tool output the model receives
//! has credential-shaped bytes replaced with placeholders — so the
//! model cannot hold a faithful view of any file that carries them. A
//! whole-file replacement of such a target would overwrite content the
//! model has never accurately seen: writing placeholders back over
//! the true values, or silently dropping them. This middleware
//! refuses the `Write` tool's whole-file calls and the shell's output
//! redirections at dispatch time, pointing the model at the surgical
//! tools instead. Command-level shell writers (`cp`, `mv`, `tee`,
//! `sed -i`) and a redirection hidden inside a quoted command
//! substitution stay outside the guard's sight: the shell remains an
//! operator-trusted write path, and the permission modes are its gate.
//! A redirect target the shell would expand — `~`, `$HOME/env.conf`,
//! `${VAR}/out`, or a computed `$(…)` or backticked word — cannot be
//! resolved statically, so the guard refuses the command rather than
//! guess at what it might name; the deny-heavy bias buys no silent
//! miss. A heredoc delimiter carries the same spellings, and one the
//! shell would compute leaves the guard no terminator it can match —
//! with the body's end unknown, no later redirection can be trusted,
//! so that command is refused on the same bias.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use crate::write_target_gate::WriteTargetGate;
use loopctl::middleware::RedactingMiddleware;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolDispatchResult;
use loopctl::middleware::ToolMiddleware;
use loopctl::middleware::ToolPipeline;
use loopctl::middleware::ToolPipelineBuilder;
use loopctl::middleware::redaction::SecretPatternSet;

/// The largest target the guard reads before refusing on size alone.
///
/// A regular file past this cap cannot have reached the model whole —
/// the read family's own windows are far smaller — so a whole-file
/// write over it would replace content the model has not seen, and
/// the guard refuses without paying for the read. The bound also
/// keeps the guard's pre-read itself bounded on the blocking pool.
const READ_CAP_BYTES: u64 = 1024 * 1024;

/// The registered name of the whole-file write tool the guard gates.
///
/// A dispatch under this name replaces its target's entire content,
/// which is exactly the operation the refusal exists to catch. The
/// guard matches by name, so a registry that renames the tool must
/// carry this constant in step or its whole-file writes pass
/// unexamined.
const WRITE_TOOL: &str = "Write";

/// The registered name of the shell tool whose redirections the guard
/// gates.
///
/// A dispatch under this name carries a free-form command, so the
/// guard scans it for output-redirection targets and checks each
/// against the same secret patterns; the name match is what routes a
/// command into that scan.
const BASH_TOOL: &str = "Bash";

/// Refuse whole-file writes whose target holds content redaction
/// hides.
///
/// Installed by [`with_redaction`] directly outside the redaction
/// pass, and only alongside it — the type and its constructor are
/// crate-private, and the seam is the one route that installs either
/// layer, so no pipeline can carry the guard without the scrubbing
/// below it. Without redaction the model does receive faithful file
/// content, so the write guard's premise — and with it the refusal —
/// does not apply.
pub(crate) struct RedactionOverwriteGuard {
    /// The secret shapes whose hiding the guard refuses to overwrite.
    ///
    /// Built from the same curated default the redaction layer
    /// scrubs with, so "the model cannot have seen these bytes" and
    /// "this write is refused" consult the same patterns.
    patterns: SecretPatternSet,
}

impl RedactionOverwriteGuard {
    /// Build the guard over the curated secret shapes.
    ///
    /// The patterns match the redaction layer's own default set —
    /// one curated set behind both halves, so the guard's promise is
    /// exactly "what redaction hides from you, you cannot overwrite
    /// wholesale".
    pub(crate) fn new() -> Self {
        Self {
            patterns: SecretPatternSet::default_common(),
        }
    }

    /// The refused-dispatch result for a guarded target.
    ///
    /// A soft error rather than a dispatch failure: the model reads
    /// the explanation, and the guidance it carries — Edit for
    /// regions seen verbatim, the user for the rest — is the only
    /// sound way forward while redaction stands between the model
    /// and the file's true bytes.
    fn refusal(ctx: &ToolDispatchContext, target: &str) -> ToolDispatchResult {
        let mut refusal = ToolDispatchResult::err(
            &ctx.tool_name,
            format!(
                "refused: `{target}` contains values the secrets redactor hides from you, \
                 so a whole-file write would replace content you have not seen verbatim. \
                 Use Edit to change only regions you have seen, or ask the user to make \
                 this change with redaction off."
            ),
            Duration::ZERO,
        );
        refusal.tool_call_id.clone_from(&ctx.call_id);
        refusal
    }

    /// The refused-dispatch result for a shell redirection whose
    /// target word the shell would expand.
    ///
    /// The guard cannot resolve `~`, `$VAR`, or a computed word to
    /// the file bash would open, so it refuses the command outright —
    /// the deny-heavy side of its bias, against the silent miss a
    /// guess would cost. The message names the spelling so the model
    /// can restate the destination as a plain path.
    fn expansion_refusal(ctx: &ToolDispatchContext, target: &str) -> ToolDispatchResult {
        let mut refusal = ToolDispatchResult::err(
            &ctx.tool_name,
            format!(
                "refused: this command redirects into `{target}`, a shell-expanded \
                 spelling (`~`, `$VAR`, `$(…)`, or backticks) the overwrite guard \
                 cannot resolve to the file it names. Restate the destination as a \
                 plain path, or ask the user to make this change."
            ),
            Duration::ZERO,
        );
        refusal.tool_call_id.clone_from(&ctx.call_id);
        refusal
    }

    /// The refused-dispatch result for a heredoc whose delimiter the
    /// shell would compute.
    ///
    /// The guard cannot expand `$(…)`, backticks, or `~` to know where
    /// the body ends, so every redirection later in the command sits
    /// behind an unknown boundary and none can be checked; the command
    /// is refused outright — the same deny-heavy bias that refuses a
    /// computed target, naming the heredoc because it, not a target
    /// word, is what the scan could not resolve.
    fn heredoc_refusal(ctx: &ToolDispatchContext, delimiter: &str) -> ToolDispatchResult {
        let mut refusal = ToolDispatchResult::err(
            &ctx.tool_name,
            format!(
                "refused: this command starts a heredoc with a computed delimiter \
                 (`{delimiter}`), so the overwrite guard cannot know where the body \
                 ends or which later redirections the shell would run. Restate the \
                 delimiter as a plain word, or ask the user to make this change."
            ),
            Duration::ZERO,
        );
        refusal.tool_call_id.clone_from(&ctx.call_id);
        refusal
    }

    /// The refused-dispatch result for a shell redirection into a
    /// guarded target.
    ///
    /// A soft error naming the target, mirroring the Write refusal:
    /// the shell's redirection is a whole-file write wearing a
    /// different spelling, so it earns the same refusal and the same
    /// guidance.
    fn bash_refusal(ctx: &ToolDispatchContext, target: &str) -> ToolDispatchResult {
        let mut refusal = ToolDispatchResult::err(
            &ctx.tool_name,
            format!(
                "refused: this command redirects into `{target}`, which contains \
                 values the secrets redactor hides from you, so the shell would \
                 overwrite content you have not seen verbatim. Ask the user to \
                 make this change with redaction off."
            ),
            Duration::ZERO,
        );
        refusal.tool_call_id.clone_from(&ctx.call_id);
        refusal
    }
}

impl Default for RedactionOverwriteGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolMiddleware for RedactionOverwriteGuard {
    fn name(&self) -> &'static str {
        "redaction-guard"
    }

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ToolDispatchContext,
        next: &'a ToolPipeline,
    ) -> Pin<Box<dyn Future<Output = ToolDispatchResult> + Send + 'a>> {
        Box::pin(async move {
            if ctx.tool_name == WRITE_TOOL
                && let Some(file_path) = crate::write_target_gate::write_family_path(&ctx.input)
                && write_refused(
                    &write_target(file_path, Path::new(&ctx.tool_context.cwd)),
                    &self.patterns,
                )
                .await
            {
                return Self::refusal(ctx, file_path);
            }
            if ctx.tool_name == BASH_TOOL
                && let Some(command) = ctx.input.get("command").and_then(|value| value.as_str())
            {
                let scan = scan_command(command);
                if let Some(delimiter) = scan.unresolved.first() {
                    return Self::heredoc_refusal(ctx, delimiter);
                }
                if let Some(expanded) = scan.targets.iter().find(|t| expansion_spelling(t)) {
                    return Self::expansion_refusal(ctx, expanded);
                }
                for target in scan.targets {
                    let resolved = write_target(&target, Path::new(&ctx.tool_context.cwd));
                    if write_refused(&resolved, &self.patterns).await {
                        return Self::bash_refusal(ctx, &target);
                    }
                }
            }
            next.dispatch(ctx).await
        })
    }
}

/// Install the write-protection layers: the target-type gate, then the
/// overwrite guard, then the redaction pass, in that order.
///
/// The one route by which any of them reaches a pipeline. The type gate
/// refuses a write whose destination is not a replaceable file, so it sits
/// outermost and answers before a target is opened; the guard refuses
/// whole-file writes over content the redactor hides, so it must sit
/// directly above the scrubbing pass that defines what is hidden, and it
/// can never be installed without it. Every host composition goes through
/// here, keeping both layers' stated preconditions true by construction
/// rather than by each caller's care.
#[must_use]
pub fn with_redaction(builder: ToolPipelineBuilder) -> ToolPipelineBuilder {
    builder
        .with_middleware(WriteTargetGate::new())
        .with_middleware(RedactionOverwriteGuard::new())
        .with_middleware(RedactingMiddleware::new(SecretPatternSet::default_common()))
}

/// Resolve a write target against the dispatch's working directory.
///
/// Absolute paths pass through and relative ones join the working
/// directory, matching how the file family resolves the same input —
/// the guard must examine the file the tool would touch, whatever
/// spelling the model used.
pub(crate) fn write_target(file_path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The file words a shell command's output redirections name.
///
/// A minimal shell lexing — words separated by whitespace and
/// operators outside quotes, quote marks stripped from words, a
/// backslash outside quotes escaping the next character — followed by
/// a pairing of each write-redirect operator (`>`, `>>`, `<>`, `>|`,
/// `&>`, `&>>`, and their fd-numbered spellings) with the word that
/// follows it. `>&` joins that set in its two file spellings: with no
/// descriptor number before it, or with `1`, the following word is a
/// file the shell opens — `>& notes` and `1>& notes` both write it —
/// while behind any other descriptor number bash refuses the command
/// as an ambiguous redirect and opens nothing, so those spellings name
/// no file. Targets that are not file words are skipped: a digit word
/// or a bare `-` names a descriptor to duplicate or close (`2>&1`,
/// `>&-`), a `&` word duplicates a descriptor, and a `(`-opening word
/// is a process substitution (`>(cmd)`). Input redirections leave no
/// target — the word after `<` is an input filename and lands as a
/// plain word — and a heredoc's body is skipped until its delimiter
/// reappears at a line start, so words and operators inside the body
/// arm nothing. A redirection hidden inside quotes — `"$(x > y)"` —
/// is invisible here, and a computed target — `> $(echo out)` —
/// leaves only the leading `$`, which the caller refuses as an
/// unresolvable spelling rather than resolving it. A heredoc
/// delimiter carrying an expansion spelling is not a target at all:
/// the scan records it on [`Scan::unresolved`] instead of arming
/// anything, and the caller refuses the whole command — no terminator
/// means no trustworthy boundary past the body.
fn scan_command(command: &str) -> Scan {
    let mut scan = Scan::default();
    let mut chars = command.chars().peekable();
    while let Some(current) = chars.next() {
        match scan.quoted {
            Quote::Single => {
                if current == '\'' {
                    scan.quoted = Quote::None;
                } else {
                    scan.word.push(current);
                }
            }
            Quote::Double => match current {
                '"' => scan.quoted = Quote::None,
                '\\' => {
                    if let Some(&escaped) = chars.peek() {
                        if matches!(escaped, '"' | '\\' | '$' | '`') {
                            scan.word.push(escaped);
                            chars.next();
                        } else {
                            scan.word.push(current);
                        }
                    } else {
                        scan.word.push(current);
                    }
                }
                _ => scan.word.push(current),
            },
            Quote::None if matches!(scan.heredoc, HeredocPhase::Body { .. }) => {
                scan.heredoc_body(current);
            }
            Quote::None => match current {
                '\n' => {
                    scan.flush_word();
                    if let HeredocPhase::Armed { delimiter } = std::mem::take(&mut scan.heredoc) {
                        scan.heredoc = HeredocPhase::Body { delimiter };
                    }
                    scan.at_line_start = true;
                }
                '\'' | '"' => scan.quoted = Quote::from_char(current),
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        scan.open_word();
                        scan.word.push(escaped);
                    }
                }
                '&' if chars.peek() == Some(&'>') => {
                    chars.next();
                    scan.scan_redirect('>', &mut chars);
                }
                ';' | '(' | ')' | '|' | '&' => {
                    scan.flush_word();
                }
                '>' | '<' => {
                    scan.scan_redirect(current, &mut chars);
                }
                _ if current.is_whitespace() => {
                    scan.flush_word();
                }
                _ => {
                    scan.open_word();
                    scan.word.push(current);
                }
            },
        }
    }
    scan.flush_word();
    scan
}

/// The file words a shell command's output redirections name.
///
/// The collected half of the command scan: the write-redirect targets
/// in command order. The word, quote, and heredoc rules that decide
/// which words these are belong to the scan itself, which the guard's
/// dispatch consults in full; this is the view the lexer's tests read.
#[must_use]
pub fn redirect_targets(command: &str) -> Vec<String> {
    scan_command(command).targets
}

/// The lexing state one command's scan carries.
///
/// Everything the target scan accumulates — the word in progress, the
/// operator waiting for its target, the heredoc being skipped, and
/// the line-start bookkeeping the heredoc terminator needs — lives
/// here so the character loop stays a dispatch over one character.
/// Where a heredoc skip stands.
///
/// One state value instead of paired flags, so the phase transitions
/// — armed by `<<`, body begun at the newline, closed by the
/// delimiter — cannot combine into shapes bash never reaches.
#[derive(Default)]
enum HeredocPhase {
    /// No heredoc is open.
    ///
    /// The resting state: every word and operator scans normally.
    #[default]
    None,
    /// `<<` was consumed; the next word is its delimiter.
    ///
    /// The flag exists only between the operator and the delimiter
    /// word, and the flush that lands the word converts it — into an
    /// armed body for a plain word, or into an unresolved heredoc for
    /// one the shell would compute.
    AwaitingDelimiter,
    /// The delimiter is consumed; its command line still scans.
    ///
    /// Words and operators on the delimiter's own line behave
    /// normally, because bash starts the body on the next line.
    Armed {
        /// The word that ends the body when it reappears at a line start.
        ///
        /// Compared against a flushed body word that began at a line
        /// start, leading tabs trimmed for the `<<-` spelling; a
        /// mid-line match is body text, not a terminator.
        delimiter: String,
    },
    /// The body is being skipped.
    ///
    /// Body characters accumulate into words only so the terminator
    /// can be recognized, and nothing inside arms or collects.
    Body {
        /// The word that ends the body when it reappears at a line start.
        ///
        /// Carried over from the armed phase unchanged; the same
        /// line-start and tab-trim rules decide the match here.
        delimiter: String,
    },
}

#[derive(Default)]
struct Scan {
    /// The word in progress, quote marks already stripped.
    ///
    /// Grown character by character and emptied by every flush; a
    /// heredoc body still grows it, but only so the terminator test
    /// has a word to compare.
    word: String,

    /// The write-redirect operator waiting for its target word.
    ///
    /// Set by [`Self::scan_redirect`] when an operator completes and
    /// cleared by the flush that lands the word it names.
    arm: Arm,

    /// The file words collected so far.
    ///
    /// In command order, one entry per write-redirect target the
    /// scan resolved — the list the guard examines.
    targets: Vec<String>,

    /// The words the scan could not resolve to anything it can check.
    ///
    /// A heredoc delimiter carrying an expansion spelling lands here
    /// at arm time: with no terminator the scan can match, nothing
    /// later in the command is trustworthy, and the caller refuses the
    /// command on that unresolvability alone.
    unresolved: Vec<String>,

    /// The quote state outside the heredoc body.
    ///
    /// A heredoc body is read verbatim, so while the body is open the
    /// scan never consults this field.
    quoted: Quote,

    /// Where the heredoc skip stands, if a heredoc is open.
    ///
    /// The phases follow bash's own reading: the delimiter word is
    /// consumed when `<<` is seen, the rest of that command line —
    /// including a redirect like `<<EOF > out` — scans normally, and
    /// the body skip takes over at the line's newline, ending when
    /// the delimiter reappears at a line start.
    heredoc: HeredocPhase,

    /// Whether the next character opens a line.
    ///
    /// True only immediately after a newline; the first character of
    /// a word copies it into `word_line_start`, which is the fact the
    /// heredoc terminator test reads.
    at_line_start: bool,

    /// Whether the word in progress began at a line start.
    ///
    /// Settled once, by the first character, and read at the flush —
    /// a delimiter word that began mid-line is body text, not a
    /// terminator.
    word_line_start: bool,
}

impl Scan {
    /// Begin a word, recording whether it opened at a line start.
    ///
    /// The first character of a word settles its line-start status,
    /// which is the fact the heredoc terminator test needs; every
    /// later character only clears the line-start flag.
    fn open_word(&mut self) {
        if self.word.is_empty() {
            self.word_line_start = self.at_line_start;
        }
        self.at_line_start = false;
    }

    /// Consume one character of a heredoc body.
    ///
    /// Words accumulate only to be tested against the delimiter;
    /// line breaks and whitespace flush them, and the flush that
    /// matches the delimiter at a line start ends the body. Operators
    /// and quote marks are literal here — bash reads the body
    /// verbatim — so nothing arms inside it.
    fn heredoc_body(&mut self, current: char) {
        if current == '\n' {
            self.flush_word();
            self.at_line_start = true;
        } else if current.is_whitespace() {
            self.flush_word();
        } else {
            self.open_word();
            self.word.push(current);
        }
    }

    /// Flush the word in progress, collecting or consuming it.
    ///
    /// Inside a heredoc body the flush only tests the terminator: a
    /// word equal to the delimiter that began at a line start ends
    /// the body, and any other word is dropped. A pending `<<` takes
    /// the word as its delimiter, unless that word carries an
    /// expansion spelling — a delimiter the shell computes, which no
    /// later line can match — and then the heredoc is recorded as
    /// unresolved instead. Otherwise an empty flush is a
    /// separator, not a word's end — `> env.conf` arms the target
    /// across the space after the operator — and a word the armed
    /// operator names is collected. The sticky arm can mis-collect a
    /// malformed command's stray word (`echo >; done` names `done`) —
    /// the refusal side of the guard's deny-heavy bias, against the
    /// missed-target side a clearing flush would cost.
    fn flush_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        if let HeredocPhase::Body { delimiter } = &self.heredoc {
            let terminated =
                self.word_line_start && self.word.trim_start_matches('\t') == *delimiter;
            self.word.clear();
            self.word_line_start = false;
            if terminated {
                self.heredoc = HeredocPhase::None;
            }
            return;
        }
        if matches!(self.heredoc, HeredocPhase::AwaitingDelimiter) {
            if expansion_spelling(&self.word) {
                self.unresolved.push(std::mem::take(&mut self.word));
                self.heredoc = HeredocPhase::None;
            } else {
                self.heredoc = HeredocPhase::Armed {
                    delimiter: std::mem::take(&mut self.word),
                };
            }
            self.word_line_start = false;
            return;
        }
        if self.arm.names_file(&self.word) {
            self.targets.push(std::mem::take(&mut self.word));
        }
        self.word.clear();
        self.word_line_start = false;
        self.arm = Arm::None;
    }

    /// Scan the remainder of a redirection operator whose lead
    /// character the caller consumed.
    ///
    /// See [`redirect_targets`] for the operator spellings and their
    /// arming rules; the heredoc family sets the delimiter-pending
    /// flag instead of an arm, so the delimiter word that follows is
    /// consumed rather than collected.
    fn scan_redirect(&mut self, lead: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        let stdout_numbered = self.word.is_empty() || self.word.as_str() == "1";
        self.flush_word();
        self.arm = Arm::None;
        match lead {
            '>' => match chars.peek() {
                Some('>') => {
                    chars.next();
                    self.arm = Arm::File;
                    if chars.peek() == Some(&'&') {
                        chars.next();
                        self.arm = Arm::None;
                    }
                }
                Some('|') => {
                    chars.next();
                    self.arm = Arm::File;
                }
                Some('&') => {
                    chars.next();
                    if stdout_numbered {
                        self.arm = Arm::FdOrFile;
                    }
                }
                Some('(') => {}
                _ => self.arm = Arm::File,
            },
            _ => match chars.peek() {
                Some('>') => {
                    chars.next();
                    self.arm = Arm::File;
                }
                Some('<') => {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        // A here-string (`<<< word`): the word is a
                        // literal operand, not a delimiter — no body
                        // follows it.
                        chars.next();
                    } else {
                        if chars.peek() == Some(&'-') {
                            chars.next();
                        }
                        self.heredoc = HeredocPhase::AwaitingDelimiter;
                    }
                }
                _ => {}
            },
        }
    }
}

/// The quote state a shell word scan carries.
///
/// The scanner threads one of these through every character of the
/// command so a redirect or operator only means what it means outside
/// quotes — a quoted `>` is a literal character, not a redirection.
#[derive(Default)]
enum Quote {
    /// Outside any quote: operators split words and arm redirects.
    ///
    /// The plain-text state every word starts and normally ends in;
    /// the scanner acts on operators only while it holds this value.
    #[default]
    None,
    /// Inside single quotes: every character is literal to the
    /// closing mark.
    ///
    /// Nothing inside — backslashes included — can escape or close
    /// the quote early, so an operator-looking text is plain bytes.
    Single,
    /// Inside double quotes: literal except the escaped `\"`, `\`,
    /// `$`, and backtick.
    ///
    /// The four backslash escapes bash still honors here are the
    /// only special readings; everything else, redirection
    /// operators included, contributes literal characters.
    Double,
}

impl Quote {
    /// The quote state an opening mark enters.
    ///
    /// A single or double mark opens its own state and every other
    /// character is treated as the double-quote opener, matching the
    /// scanner's caller having already recognized a quote begins.
    fn from_char(mark: char) -> Self {
        match mark {
            '\'' => Self::Single,
            _ => Self::Double,
        }
    }
}

/// What the word following a redirection operator contributes.
///
/// The scanner carries one of these per operator it has read, naming
/// whether the next word is the file that operator opens. The two file
/// arms differ because bash reads the same word two ways: after a plain
/// `>` any word is a file, while after `>&` a word can be a descriptor
/// to duplicate instead.
#[derive(Default)]
enum Arm {
    /// No write-redirect operator is waiting for a target.
    ///
    /// The resting state between words: whatever word follows opens
    /// nothing, so the scanner only accumulates it.
    #[default]
    None,
    /// A whole-file write operator awaits the file it opens.
    ///
    /// Set by the plain `>` family — the next word is unambiguously
    /// the file bash will create or truncate, whatever it spells.
    File,
    /// The stdout redirection `>&`, spelled with no descriptor number or
    /// with `1`.
    ///
    /// A descriptor word after it — a run of digits, or a bare `-` —
    /// duplicates or closes a descriptor, and any other word is a file
    /// bash opens; a different number before the operator makes bash
    /// refuse the command, so that spelling is never armed.
    FdOrFile,
}

impl Arm {
    /// Whether a flushed word is the file the armed operator opens.
    ///
    /// A `&` word duplicates a descriptor and a `(`-opening word is a
    /// process substitution, so neither is ever a file. Under
    /// [`Arm::FdOrFile`] a descriptor word names a descriptor to
    /// duplicate or close rather than a file to open.
    fn names_file(&self, word: &str) -> bool {
        if word.starts_with('&') || word.starts_with('(') {
            return false;
        }
        match self {
            Self::None => false,
            Self::File => true,
            Self::FdOrFile => word != "-" && !word.bytes().all(|byte| byte.is_ascii_digit()),
        }
    }
}

/// Whether a scanned target word is one the shell would expand.
///
/// `~` at the word's start names the home directory, while `$` or a
/// backtick anywhere introduces a variable, brace, or command
/// expansion the lexical scan cannot run. The two triggers differ in
/// position because bash differs: a tilde mid-word is literal, so only
/// a leading one counts, while a backtick expands wherever it sits —
/// and it counts on containment rather than a leading mark because the
/// scanner treats it as ordinary word content, so a target written as
/// a backticked substitution splits at the substitution's own
/// whitespace and only the fragment carrying the opener is ever
/// collected. Either way the file bash would open is not the path the
/// guard could join, so the caller refuses rather than guess. A
/// single-quoted `$word` the shell would treat as literal is refused
/// too — the scanner strips quote marks, so the two spellings are
/// indistinguishable, and the false refusal is the bias this module
/// already claims.
fn expansion_spelling(word: &str) -> bool {
    word.starts_with('~') || word.contains('$') || word.contains('`')
}

/// What one opened target yields to the guard's read.
///
/// The three outcomes of opening a path and asking the opened
/// descriptor what it is: content to inspect, a target class the guard
/// must not read, or a regular file too large to have reached the
/// model whole.
enum TargetRead {
    /// The opened descriptor is a regular file at or under the cap;
    /// these are its bytes, read through the very descriptor the
    /// classification settled on.
    Bytes(Vec<u8>),

    /// The opened descriptor is not a regular file — a FIFO, device,
    /// socket, or directory carries no file content whose redaction
    /// could hide anything, and no read of it is attempted.
    NotRegular,

    /// A regular file past [`READ_CAP_BYTES`], refused on size alone:
    /// the read family's windows are far smaller, so the model cannot
    /// have seen it whole, and the guard declines to pay for the read.
    OverCap,
}

/// Open `target` for reading the way the guard's classification needs.
///
/// On unix the open carries `O_NONBLOCK`, which is what lets the guard
/// ask an unknown entry what it is: a plain open of a FIFO waits in
/// the open itself for a writer that may never arrive, and no runtime
/// can cancel a wait a dropped future still occupies. For a regular
/// file — the only thing the guard ever reads — the flag changes
/// nothing.
///
/// # Errors
///
/// The caller's filesystem error, verbatim; the callers below fold any
/// of them into a refusal rather than guessing the target is
/// secret-free.
#[cfg(unix)]
fn open_for_read(target: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(target)
}

/// The non-unix open: the endpoint class a non-blocking open exists to
/// tame is a unix one, so the plain open stands here.
#[cfg(not(unix))]
fn open_for_read(target: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(target)
}

/// Classify and read `target` through one descriptor.
///
/// The open comes first and the metadata is asked of the opened file,
/// which on unix is an `fstat` of the descriptor itself. The entry at
/// the path may be swapped for another kind at any moment, but the
/// descriptor names one inode, so the kind the guard classified is
/// necessarily the kind it reads — check and use cannot diverge.
///
/// # Errors
///
/// The caller's filesystem error, verbatim; `write_refused` folds any of
/// them into a refusal rather than guessing the target is secret-free.
fn open_and_read(target: &Path) -> std::io::Result<TargetRead> {
    use std::io::Read as _;

    let file = open_for_read(target)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(TargetRead::NotRegular);
    }
    if metadata.len() > READ_CAP_BYTES {
        return Ok(TargetRead::OverCap);
    }
    let mut bytes = Vec::new();
    file.take(READ_CAP_BYTES).read_to_end(&mut bytes)?;
    Ok(TargetRead::Bytes(bytes))
}

/// Whether a whole-file write to `target` must be refused.
///
/// The target is opened first — on unix non-blockingly, so a FIFO or
/// device can stall neither the open nor a read that never happens —
/// and its kind is settled from that opened descriptor, which is what
/// keeps a swap of the entry from sending the guard's read onto a
/// pipe. Only a regular file carries content whose redaction could
/// hide something, so a non-regular or unopenable target steps aside
/// for the tool's own path. A regular file over [`READ_CAP_BYTES`] is
/// refused on size: the read family's windows are far smaller, so the
/// model cannot have seen it whole. The bytes are read on the blocking
/// pool, bounded by the cap, through the same descriptor, and a copy
/// is scrubbed: any substitution means the redaction layer rewrites
/// this file's content before the model sees it, so the model's
/// context cannot be a faithful basis for replacing the whole file.
/// Bytes that cannot be read or decoded are the write tool's own path
/// to decide; the guard steps aside, and the staleness machinery
/// still governs whatever the tool does next.
async fn write_refused(target: &Path, patterns: &SecretPatternSet) -> bool {
    let owned = target.to_path_buf();
    let read = tokio::task::spawn_blocking(move || open_and_read(&owned)).await;
    match read {
        Ok(Ok(TargetRead::Bytes(bytes))) => {
            let Ok(text) = String::from_utf8(bytes) else {
                return false;
            };
            let mut copy = text;
            patterns.scrub(&mut copy) > 0
        }
        Ok(Ok(TargetRead::OverCap)) => true,
        Ok(Ok(TargetRead::NotRegular) | Err(_)) | Err(_) => false,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;

    /// Seed a file whose `api_key=` line the curated patterns match.
    ///
    /// The sixteen-character value clears the pattern's length floor,
    /// so a scrub over the file's bytes substitutes at least once —
    /// the fixture for every refused-write scenario below.
    fn secret_file(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("env.conf");
        std::fs::write(&path, "api_key=abcdefghijklmnop\nport = 8080\n")
            .expect("seed the secret-bearing file");
        path
    }

    #[tokio::test]
    async fn a_target_the_patterns_match_refuses_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = secret_file(dir.path());
        assert!(
            write_refused(&path, &SecretPatternSet::default_common()).await,
            "a file whose redaction would substitute content refuses the whole-file write"
        );
    }

    #[tokio::test]
    async fn a_target_no_pattern_matches_allows_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plain.conf");
        std::fs::write(&path, "port = 8080\nhost = \"localhost\"\n").expect("seed plain file");
        assert!(
            !write_refused(&path, &SecretPatternSet::default_common()).await,
            "a file redaction serves verbatim stays writable"
        );
    }

    #[tokio::test]
    async fn a_missing_target_steps_aside_for_the_tool_to_decide() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            !write_refused(
                &dir.path().join("absent.conf"),
                &SecretPatternSet::default_common()
            )
            .await,
            "an unreadable target is the write tool's own path, not the guard's"
        );
    }

    #[tokio::test]
    async fn a_non_regular_target_steps_aside_without_reading_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe.conf");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "the fifo fixture exists");
        let began = std::time::Instant::now();
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            write_refused(&fifo, &SecretPatternSet::default_common()),
        )
        .await
        .expect("a non-regular target resolves fast — the guard never reads it");
        assert!(
            !refused,
            "a fifo carries no file content for redaction to hide"
        );
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "the opened descriptor settles the target's kind without a read"
        );
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            write_refused(Path::new("/dev/zero"), &SecretPatternSet::default_common()),
        )
        .await
        .expect("a character device resolves fast");
        assert!(
            !refused,
            "a device is not a file whose content the guard examines"
        );
    }

    #[tokio::test]
    async fn an_over_cap_target_refuses_on_size_without_reading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.conf");
        let line = "just ordinary config lines\n";
        let repeats =
            usize::try_from(READ_CAP_BYTES / line.len() as u64).expect("cap fits usize") + 2;
        std::fs::write(&path, line.repeat(repeats)).expect("seed the oversized file");
        assert!(
            write_refused(&path, &SecretPatternSet::default_common()).await,
            "a target past the read cap cannot have reached the model whole, so the \
             whole-file write is refused on size alone"
        );
    }

    #[tokio::test]
    async fn a_symlinked_secret_target_is_still_examined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = secret_file(dir.path());
        let link = dir.path().join("link.conf");
        std::os::unix::fs::symlink(&real, &link).expect("symlink the secret file");
        assert!(
            write_refused(&link, &SecretPatternSet::default_common()).await,
            "the guard follows a symlink to the file the tool would touch — the same \
             path resolution the write layer applies"
        );
    }

    #[test]
    fn expansion_spellings_are_the_words_the_guard_refuses() {
        for word in [
            "~/env.conf",
            "$HOME/env.conf",
            "${VAR}/out",
            "$V",
            "out${V}",
            "$(echo x)",
            "`echo x`",
            "out`id`",
        ] {
            assert!(
                expansion_spelling(word),
                "`{word}` is a spelling the shell expands and the guard cannot resolve"
            );
        }
        for word in [
            "env.conf",
            "./env.conf",
            "/etc/env.conf",
            "my file.conf",
            "a~b.conf",
        ] {
            assert!(
                !expansion_spelling(word),
                "`{word}` is a plain path the guard resolves lexically"
            );
        }
        // A `$` anywhere in the word expands in bash — including
        // mid-word, which the deny-heavy arm refuses rather than
        // guesses at. A mid-word `~` is the one spelling bash leaves
        // literal, so it stays on the arming side of the guard.
        assert!(expansion_spelling("a$b"));
    }

    #[test]
    fn a_backticked_target_reaches_the_guard_as_one_expandable_fragment() {
        assert_eq!(
            redirect_targets("printf x > `echo ./env.conf`"),
            vec!["`echo".to_string()],
            "the substitution splits at its own whitespace, and the collected \
             fragment carries the opening mark"
        );
        assert!(
            expansion_spelling("`echo"),
            "the fragment names nothing the guard could resolve, so it refuses"
        );
    }

    #[test]
    fn a_relative_target_resolves_against_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        secret_file(dir.path());
        let resolved = write_target("env.conf", dir.path());
        assert_eq!(
            resolved,
            dir.path().join("env.conf"),
            "a relative spelling resolves onto the file the tool would touch"
        );
        let absolute = dir.path().join("env.conf");
        assert_eq!(
            write_target(
                absolute.to_str().expect("utf8 path"),
                Path::new("/elsewhere")
            ),
            absolute,
            "an absolute spelling passes through unchanged"
        );
    }

    #[test]
    fn every_write_redirect_spelling_names_its_target() {
        for (command, expected) in [
            ("printf x > env.conf", &["env.conf"][..]),
            ("printf x >env.conf", &["env.conf"]),
            ("printf x >> env.conf", &["env.conf"]),
            ("printf x 2> env.conf", &["env.conf"]),
            ("printf x &> env.conf", &["env.conf"]),
            ("printf x &>> env.conf", &["env.conf"]),
            ("printf x >| env.conf", &["env.conf"]),
            ("printf x <> env.conf", &["env.conf"]),
            ("printf x >& env.conf", &["env.conf"]),
            ("printf x >&env.conf", &["env.conf"]),
            ("printf x 1>& env.conf", &["env.conf"]),
            ("printf x > 12", &["12"]),
            ("a > one.log 2>> two.log", &["one.log", "two.log"]),
            ("--opt=x>y", &["y"]),
            ("echo $(printf x > env.conf)", &["env.conf"]),
        ] {
            let expected: Vec<String> = expected.iter().map(|word| (*word).to_string()).collect();
            assert_eq!(
                redirect_targets(command),
                expected,
                "the spelling `{command}` names its write target"
            );
        }
    }

    #[test]
    fn quoted_redirect_targets_lose_their_quotes() {
        assert_eq!(
            redirect_targets("printf x > \"my file.conf\""),
            vec!["my file.conf".to_string()],
            "a double-quoted target keeps its spaces as one word"
        );
        assert_eq!(
            redirect_targets("printf x >'env.conf'"),
            vec!["env.conf".to_string()],
            "a single-quoted target loses its marks"
        );
    }

    #[test]
    fn non_file_words_never_become_targets() {
        for command in [
            "cmd 2>&1",
            "cmd >&2",
            "cmd 1>&2",
            "cmd >&12",
            "cmd >&-",
            "cmd 2>& notes",
            "cmd 3>& notes",
            "cmp >(sort a) >(sort b)",
            "cat < input.txt",
            "cat <<EOF",
            "cat <<-EOF",
            "cat <<< greeting",
            "echo hello",
            "echo \\> literal",
            "echo \" > quoted\"",
        ] {
            assert_eq!(
                redirect_targets(command),
                Vec::<String>::new(),
                "the command `{command}` names no write target"
            );
        }
    }

    #[test]
    fn a_heredoc_body_still_redirects_to_its_file() {
        assert_eq!(
            redirect_targets("cat <<EOF > env.conf"),
            vec!["env.conf".to_string()],
            "the heredoc's delimiter is skipped and the redirect keeps its target"
        );
    }

    #[test]
    fn heredoc_body_words_never_arm_a_target() {
        // The body is skipped until the delimiter reappears at a line
        // start, so a redirect-shaped interior line names nothing —
        // bash writes only the files the command line itself names.
        assert_eq!(
            redirect_targets("cat <<EOF > out\nsee a > b\nEOF"),
            vec!["out".to_string()],
            "the body's interior redirect arms nothing; the command line's target stands"
        );
        assert_eq!(
            redirect_targets("cat <<EOF\ninner > b\nEOF"),
            Vec::<String>::new(),
            "a body-only redirect names no file"
        );
        assert_eq!(
            redirect_targets("cat <<EOF\nmid EOF line > f\nEOF"),
            Vec::<String>::new(),
            "the delimiter mid-line does not end the body, so the following word arms nothing"
        );
        assert_eq!(
            redirect_targets("cat <<EOF\nbody\nEOF\necho x > real.conf"),
            vec!["real.conf".to_string()],
            "after the terminator, redirects arm again"
        );
        assert_eq!(
            redirect_targets("cat <<-EOF\n\tbody > b\n\tEOF\necho y > real.log"),
            vec!["real.log".to_string()],
            "a tab-indented terminator closes a << heredoc and later redirects arm"
        );
    }

    #[test]
    fn computed_heredoc_delimiters_are_reported_unresolved() {
        for command in [
            "cat <<$(echo EOF)\nbody\nEOF\necho x >> ./credentials",
            "cat <<`echo EOF`\nbody\nEOF\necho x >> ./credentials",
            "cat <<~TARGET\nbody\nTARGET",
            "cat <<${V}\nbody\nX",
        ] {
            let scan = scan_command(command);
            assert!(
                !scan.unresolved.is_empty(),
                "the command `{command}` arms a delimiter the shell computes, which no \
                 terminator test can match"
            );
        }
    }

    #[test]
    fn a_computed_delimiter_blinds_nothing_past_it() {
        let scan = scan_command("cat <<$(echo EOF)\nbody\nEOF\necho x >> ./credentials");
        assert_eq!(
            scan.unresolved.first().map(String::as_str),
            Some("$"),
            "the recorded fragment carries the opener of the substitution"
        );
        assert_eq!(
            scan.targets,
            vec!["./credentials".to_string()],
            "the redirect the old scan swallowed past the unknowable body is visible now"
        );
    }

    #[test]
    fn a_quoted_heredoc_delimiter_never_lands_in_unresolved() {
        let scan = scan_command("cat <<'EOF'\ninner > b\nEOF\necho y > real.conf");
        assert!(
            scan.unresolved.is_empty(),
            "a quoted delimiter is a plain word the shell does not compute"
        );
        assert_eq!(
            scan.targets,
            vec!["real.conf".to_string()],
            "the body is still skipped and the trailing redirect still names its file"
        );
    }

    #[test]
    fn operators_inside_quotes_stay_literal() {
        assert_eq!(
            redirect_targets("echo \"$(cat a > b)\""),
            Vec::<String>::new(),
            "a redirect inside double quotes is invisible to the scan — the documented residual"
        );
    }

    #[test]
    fn a_redirection_inside_an_unquoted_substitution_is_still_seen() {
        // The residual the module doc names covers only the quoted
        // spelling: outside quotes the scanner still splits on the
        // operator and arms the word that follows, so the guarded
        // target inside a command substitution is refused.
        assert_eq!(
            redirect_targets("echo $(printf x > env.conf)"),
            vec!["env.conf".to_string()],
            "an unquoted substitution hides nothing from the scan"
        );
    }

    #[test]
    fn a_computed_redirect_target_is_never_resolved() {
        // The other half of the substitution blind spot: when the
        // substitution is the target word, the scan cannot run it, so it
        // collects the leading `$` and the file the shell opens stays
        // outside the guard's sight — a miss, like the quoted spelling.
        assert_eq!(
            redirect_targets("cat a > $(echo out)"),
            vec!["$".to_string()],
            "a computed target resolves to nothing the guard can check"
        );
    }
}
