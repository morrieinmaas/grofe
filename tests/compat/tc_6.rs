use crate::tp::check;

/// A floating pane's border takes tmux 3.7b's stock rows: a click focuses
/// the pane and clears the mark, a drag moves it by the top edge and
/// resizes it by the others, the release fires `MouseDragEnd1Border` or
/// `MouseDragEnd1Pane` where it lands, a binding of the user's own wins,
/// and a pane that asked for the mouse gets clicks inside it only. With the
/// stock rows unbound a border (floating or tiled) does nothing, and a
/// Meta, Control or Shift variant acts only if it is bound itself.
#[test]
fn a_floating_pane_border_matches_its_golden() {
    check("tc_6", "float-border-mouse", &[]);
}
