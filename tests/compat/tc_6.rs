use crate::tp::check;

/// A floating pane's border takes tmux 3.7b's stock rows: a click focuses
/// the pane and clears the mark, a drag moves it by the top edge and
/// resizes it by the others, the release fires `MouseDragEnd1Border` or
/// `MouseDragEnd1Pane` where it lands, a binding of the user's own wins,
/// and a pane that asked for the mouse gets clicks inside it only.
#[test]
fn a_floating_pane_border_matches_its_golden() {
    check(
        "tc_6",
        "float-border-mouse",
        &[
            (
                "$ display -p -t main 'unbound: #{pane_id}'",
                "the built-in model stands in for tmux's stock mouse rows, so \
                 unbinding MouseDown1Border does not stop it focusing the pane",
            ),
            (
                "$ display -p -t main:1.2 'unbound: #{pane_left} #{pane_top} #{pane_width} #{pane_height}'",
                "nor does unbinding MouseDrag1Border stop the drag (tiled borders too)",
            ),
        ],
    );
}
