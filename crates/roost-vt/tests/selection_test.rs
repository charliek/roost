#![cfg(feature = "ffi")]

use roost_vt::{RenderState, Terminal, TerminalOptions, TerminalSelection};

fn terminal() -> (Terminal, RenderState) {
    (
        Terminal::new(TerminalOptions {
            cols: 20,
            rows: 5,
            max_scrollback: 20,
        })
        .expect("terminal"),
        RenderState::new().expect("render state"),
    )
}

#[test]
fn committed_selection_extracts_exact_text() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"hello world");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (4, 0)));
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, 20, 5)
            .expect("selected text"),
        Some("hello".into())
    );
}

#[test]
fn clear_returns_to_empty_snapshot() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"abc");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (2, 0)));
    assert!(selection.clear());
    assert_eq!(
        selection
            .snapshot(&terminal, &mut render_state, 20, 5)
            .expect("snapshot"),
        None
    );
}

#[test]
fn out_of_range_begin_clears_stale_selection() {
    let (mut terminal, _) = terminal();
    terminal.vt_write(b"abc");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (2, 0)));
    assert!(!selection.begin(&terminal, 0, u16::MAX));
    assert!(!selection.is_active());
}

#[test]
fn multi_row_selection_yields_shared_spans_and_text() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"abc\r\ndef\r\nghi");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (1, 0), (1, 2)));
    assert_eq!(
        selection.visible_spans(&terminal, 20, 5),
        vec![
            roost_vt::SelectionSpan {
                row: 0,
                col0: 1,
                col1: 20,
            },
            roost_vt::SelectionSpan {
                row: 1,
                col0: 0,
                col1: 20,
            },
            roost_vt::SelectionSpan {
                row: 2,
                col0: 0,
                col1: 2,
            },
        ]
    );
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, 20, 5)
            .expect("selected text"),
        Some("bc\ndef\ngh".into())
    );
}

#[test]
fn drag_is_hidden_until_it_moves_and_invalid_update_clears_it() {
    let (terminal, _) = terminal();
    let mut selection = TerminalSelection::new();
    assert!(selection.begin(&terminal, 2, 1));
    assert!(selection.visible_spans(&terminal, 20, 5).is_empty());
    assert!(selection.update(&terminal, 4, 1));
    assert_eq!(
        selection.visible_spans(&terminal, 20, 5),
        vec![roost_vt::SelectionSpan {
            row: 1,
            col0: 2,
            col1: 5,
        }]
    );
    assert!(!selection.update(&terminal, 4, u16::MAX));
    assert!(!selection.is_active());
}
