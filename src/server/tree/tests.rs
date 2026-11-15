use super::*;

/// `O` in customize-mode, which has no orders, used to index an
/// empty slice and take the server down.
#[test]
fn o_cycles_the_orders_and_does_nothing_without_any() {
    let tree = Kind::Clients.orders();
    assert_eq!(next_order(tree, Order::Name), Some(Order::Size));
    assert_eq!(next_order(tree, Order::Activity), Some(Order::Name));
    assert_eq!(next_order(tree, Order::Z), Some(Order::Name));
    assert_eq!(next_order(&[], Order::Name), None);
}

/// A current line past the end of the list (everything it showed went
/// away) is not kept as it is: it is clamped back onto the list.
#[test]
fn a_stale_current_line_is_clamped() {
    let lines: Vec<Line> = (0..2).map(|i| line(Tag::Pane(PaneId(i)))).collect();
    assert!(!still_current(&lines, 5, None));
    assert!(still_current(&lines, 1, Some(Tag::Pane(PaneId(1)))));
    assert_eq!(set_current(&lines, None, 5, 0, 10), (1, 0));
}

#[test]
fn line_keys_are_digits_then_meta_letters() {
    assert_eq!(line_key(0).as_deref(), Some("0"));
    assert_eq!(line_key(9).as_deref(), Some("9"));
    assert_eq!(line_key(10).as_deref(), Some("M-a"));
    assert_eq!(line_key(35).as_deref(), Some("M-z"));
    assert_eq!(line_key(36), None);
    let key = |name| crate::server::input::parse_key(name).expect("key");
    let keys: Vec<Option<String>> = (0..5).map(line_key).collect();
    assert_eq!(line_of_key(&key("3"), &keys), Some(3));
    assert_eq!(line_of_key(&key("7"), &keys), None);
    let keys: Vec<Option<String>> = (0..20).map(line_key).collect();
    assert_eq!(line_of_key(&key("M-b"), &keys), Some(11));
    assert_eq!(line_of_key(&key("b"), &keys), None);
}

/// `mode_tree_set_height` in a 23-row pane: two thirds of it, half when
/// the list is shorter, the whole pane when that leaves under 10 rows.
#[test]
fn the_list_takes_mode_trees_height() {
    use PreviewMode::*;
    assert_eq!(list_height(23, 5, Normal), 11);
    assert_eq!(list_height(23, 27, Normal), 14);
    assert_eq!(list_height(15, 3, Normal), 15);
    assert_eq!(list_height(12, 20, Normal), 12);
    // No preview: the whole pane; a big one: a quarter, 2 at least.
    assert_eq!(list_height(23, 5, Off), 23);
    assert_eq!(list_height(24, 50, Big), 6);
    assert_eq!(list_height(24, 3, Big), 3);
    assert_eq!(list_height(24, 1, Big), 2);
}

/// `window_tree_draw_session`'s strip at 76 columns: side by side while
/// each gets 24 columns, else three with `<`/`>` and the current one in
/// view, scrolled by the offset within its bounds.
#[test]
fn previews_share_the_width_as_tmux_does() {
    let g = strip(2, 0, 76, 0).expect("fits");
    assert_eq!((g.start, g.end, g.left, g.right), (0, 2, false, false));
    assert_eq!((g.each, g.remaining), (38, 0));
    let g = strip(7, 3, 76, 0).expect("fits");
    assert_eq!((g.start, g.end, g.left, g.right), (2, 5, true, true));
    assert_eq!((g.each, g.remaining), (23, 1));
    let g = strip(7, 3, 76, 9).expect("fits");
    assert_eq!((g.start, g.end, g.offset), (4, 7, 2));
    assert!(g.left && !g.right);
    let g = strip(7, 0, 76, -3).expect("fits");
    assert_eq!((g.start, g.offset), (0, 0));
    assert!(strip(0, 0, 76, 0).is_none());
    assert!(strip(3, 0, 0, 0).is_none());
}

fn line(tag: Tag) -> Line {
    Line {
        tag,
        session: None,
        depth: 0,
        last: false,
        flat: true,
        parent: None,
        has_children: false,
        expanded: true,
        name: String::new(),
        text: String::new(),
        align: false,
        plain: false,
        no_tag: false,
        as_parent: false,
    }
}

#[test]
fn the_current_line_is_found_again_by_its_item() {
    let lines: Vec<Line> = (0..20).map(|i| line(Tag::Pane(PaneId(i)))).collect();
    assert_eq!(
        set_current(&lines, Some(Tag::Pane(PaneId(15))), 0, 0, 10),
        (15, 6)
    );
    assert_eq!(
        set_current(&lines, Some(Tag::Pane(PaneId(3))), 9, 5, 10),
        (3, 0)
    );
    // A vanished item keeps the line, clamped to the list.
    assert_eq!(
        set_current(&lines, Some(Tag::Pane(PaneId(99))), 25, 0, 10),
        (19, 10)
    );
    assert_eq!(
        set_current(&lines, Some(Tag::Pane(PaneId(99))), 4, 2, 10),
        (4, 2)
    );
    // Before the first build the height is 0 and nothing scrolls.
    assert_eq!(
        set_current(&lines, Some(Tag::Pane(PaneId(15))), 0, 0, 0),
        (15, 0)
    );
}
