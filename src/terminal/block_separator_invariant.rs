use super::*;
use proptest::prelude::*;

const PROMPT_START: &str = "\x1b]133;A\x1b\\";

fn owns_prompt(term: &Terminal, row: usize) -> bool {
    term.visible_rows()
        .nth(row)
        .and_then(|cells| cells.first())
        .is_some_and(|c| c.ch == '$')
}

fn stale_separators(term: &Terminal) -> Vec<usize> {
    term.block_separator_viewport_rows()
        .into_iter()
        .filter(|&row| !owns_prompt(term, row))
        .collect()
}

fn prompt(term: &mut Terminal, command: &str) {
    term.feed(format!("\r{PROMPT_START}$ {command}\r\n").as_bytes());
}

#[test]
fn clear_then_in_place_progress_leaves_no_separator_on_a_row_that_lost_its_prompt() {
    let mut term = Terminal::new(120, 65);
    prompt(&mut term, "clear");
    prompt(&mut term, "cd nix");
    prompt(&mut term, "git pull origin main");
    term.feed(b"Already up to date.\r\n\r\n");
    prompt(&mut term, "ls -ltra");
    for i in 0..50 {
        term.feed(format!("entry {i}\r\n").as_bytes());
    }
    prompt(&mut term, "clear");
    term.feed(b"\x1b[H\x1b[2J\x1b[3J");
    term.feed(format!("{PROMPT_START}$ nix run .#rebuild\r\n").as_bytes());
    term.feed(b"copying '/Users/luis.d/code/github/pleme-io/nix/' to the store");
    for i in 0..20 {
        term.feed(format!("\r\x1b[K[{i}/40 built] evaluating").as_bytes());
    }
    term.feed(b"\r\x1b[K");

    assert_eq!(stale_separators(&term), Vec::<usize>::new());
    assert_eq!(term.block_separator_viewport_rows(), vec![0]);
}

#[test]
fn a_line_editor_repaint_keeps_the_live_prompt_separator() {
    let mut term = Terminal::new(40, 10);
    prompt(&mut term, "echo");
    term.feed(b"out\r\n");
    term.feed(format!("\r{PROMPT_START}$ ").as_bytes());
    for typed in ["g", "gi", "git"] {
        term.feed(format!("\r\x1b[J$ {typed}").as_bytes());
    }
    assert_eq!(term.block_separator_viewport_rows(), vec![0, 2]);
}

#[test]
fn consecutive_empty_prompts_draw_no_separator_between_them() {
    let mut term = Terminal::new(40, 30);
    for _ in 0..22 {
        prompt(&mut term, "");
    }
    term.feed(format!("\r{PROMPT_START}$ ").as_bytes());
    assert_eq!(term.block_separator_viewport_rows(), vec![0]);
    term.feed(b"\r\n");
    prompt(&mut term, "ls");
    term.feed(b"a b\r\n");
    term.feed(format!("\r{PROMPT_START}$ ").as_bytes());
    assert_eq!(term.block_separator_viewport_rows(), vec![0, 25]);
}

#[test]
fn a_prompt_below_the_viewport_top_takes_its_neighbour_from_scrollback() {
    let mut term = Terminal::new(40, 5);
    for _ in 0..8 {
        prompt(&mut term, "");
    }
    assert_eq!(term.block_separator_viewport_rows(), Vec::<usize>::new());
}

#[test]
fn inserted_and_deleted_lines_carry_the_separator_with_its_prompt() {
    let mut term = Terminal::new(40, 12);
    term.feed(b"one\r\ntwo\r\n");
    prompt(&mut term, "echo");
    term.feed(b"\x1b[1;10r\x1b[1;1H\x1b[2L\x1b[r\x1b[12;1H");
    assert_eq!(term.block_separator_viewport_rows(), vec![4]);
    term.feed(b"\x1b[1;10r\x1b[1;1H\x1b[3M\x1b[r\x1b[12;1H");
    assert_eq!(term.block_separator_viewport_rows(), vec![1]);
    assert_eq!(stale_separators(&term), Vec::<usize>::new());
}

#[derive(Debug, Clone)]
enum Op {
    Prompt { repaints: u8 },
    Output { lines: u8 },
    Progress { steps: u8 },
    EraseBelow { row: u8, col: u8 },
    EraseAbove { row: u8, col: u8 },
    Clear { scrollback: bool },
    InsertLines { top: u8, span: u8, at: u8, n: u8 },
    DeleteLines { top: u8, span: u8, at: u8, n: u8 },
    AltScreen,
    Resize { cols: u8, rows: u8 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u8..4).prop_map(|repaints| Op::Prompt { repaints }),
        (1u8..30).prop_map(|lines| Op::Output { lines }),
        (1u8..8).prop_map(|steps| Op::Progress { steps }),
        (0u8..40, 1u8..30).prop_map(|(row, col)| Op::EraseBelow { row, col }),
        (0u8..40, 1u8..30).prop_map(|(row, col)| Op::EraseAbove { row, col }),
        any::<bool>().prop_map(|scrollback| Op::Clear { scrollback }),
        (0u8..20, 1u8..20, 0u8..20, 1u8..6).prop_map(|(top, span, at, n)| Op::InsertLines {
            top,
            span,
            at,
            n
        }),
        (0u8..20, 1u8..20, 0u8..20, 1u8..6).prop_map(|(top, span, at, n)| Op::DeleteLines {
            top,
            span,
            at,
            n
        }),
        Just(Op::AltScreen),
        (20u8..90, 5u8..30).prop_map(|(cols, rows)| Op::Resize { cols, rows }),
    ]
}

fn park(term: &mut Terminal) {
    term.feed(format!("\x1b[{};1H\r\n", term.rows()).as_bytes());
}

fn region(term: &Terminal, top: u8, span: u8, at: u8) -> (usize, usize, usize) {
    let rows = term.rows();
    let top = usize::from(top) % rows;
    let bottom = (top + usize::from(span)).min(rows - 1);
    let at = top + usize::from(at) % (bottom - top + 1);
    (top + 1, bottom + 1, at + 1)
}

fn apply(term: &mut Terminal, op: &Op) {
    match *op {
        Op::Prompt { repaints } => {
            term.feed(format!("\r{PROMPT_START}$ ").as_bytes());
            for k in 0..repaints {
                term.feed(format!("\r\x1b[J$ cmd{k}").as_bytes());
            }
            term.feed(b"\r\n");
        }
        Op::Output { lines } => {
            for i in 0..lines {
                term.feed(format!("\rout {i}\r\n").as_bytes());
            }
        }
        Op::Progress { steps } => {
            for i in 0..steps {
                term.feed(format!("\r\x1b[Kprogress {i}").as_bytes());
            }
            term.feed(b"\r\x1b[Kdone\r\n");
        }
        Op::EraseBelow { row, col } => {
            term.feed(format!("\x1b[{};{}H\x1b[0J", row + 1, col + 1).as_bytes());
            park(term);
        }
        Op::EraseAbove { row, col } => {
            term.feed(format!("\x1b[{};{}H\x1b[1J", row + 1, col + 1).as_bytes());
            park(term);
        }
        Op::Clear { scrollback } => {
            term.feed(b"\x1b[H\x1b[2J");
            if scrollback {
                term.feed(b"\x1b[3J");
            }
        }
        Op::InsertLines { top, span, at, n } => {
            let (t, b, a) = region(term, top, span, at);
            term.feed(format!("\x1b[{t};{b}r\x1b[{a};1H\x1b[{n}L\x1b[r").as_bytes());
            park(term);
        }
        Op::DeleteLines { top, span, at, n } => {
            let (t, b, a) = region(term, top, span, at);
            term.feed(format!("\x1b[{t};{b}r\x1b[{a};1H\x1b[{n}M\x1b[r").as_bytes());
            park(term);
        }
        Op::AltScreen => {
            term.feed(b"\x1b[?1049h\x1b[2J$ tui\x1b[?1049l");
        }
        Op::Resize { cols, rows } => {
            term.resize(usize::from(cols), usize::from(rows));
            park(term);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn every_block_separator_sits_on_the_row_that_holds_its_prompt(
        ops in proptest::collection::vec(op(), 1..40),
    ) {
        let mut term = Terminal::new(60, 20);
        for op in &ops {
            apply(&mut term, op);
            let stale = stale_separators(&term);
            prop_assert!(stale.is_empty(), "after {:?}: separators on rows {:?} hold no prompt", op, stale);
            let seps = term.block_separator_viewport_rows();
            for row in 1..term.rows() {
                let opens_a_block = owns_prompt(&term, row) && !owns_prompt(&term, row - 1);
                prop_assert_eq!(
                    seps.contains(&row),
                    opens_a_block,
                    "after {:?}: row {} separator must exist iff its prompt follows output; seps {:?}",
                    op,
                    row,
                    seps
                );
            }
        }
    }
}
