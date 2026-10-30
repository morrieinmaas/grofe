//! `-O order` and `-r` of the commands that list things, as tmux 3.7b's
//! `sort.c` sorts: with no `-O` the list stays as tmux keeps it (`-r` then
//! does nothing), `-O order` keeps it too and `-r` reverses it, any other
//! order sorts, a tie going to the item's name, and `-r` reverses the
//! result.

use std::cmp::Ordering;

use crate::cmd::SortFlags;

/// tmux's `enum sort_order`, minus its end marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Activity,
    Creation,
    Index,
    Modifier,
    Name,
    /// The list as tmux keeps it.
    Natural,
    Size,
    Z,
}

impl Order {
    /// `sort_order_from_string`: names in any case; `key` is `index` and
    /// `title` is `name`.
    fn from_name(name: &str) -> Option<Order> {
        Some(match name.to_ascii_lowercase().as_str() {
            "activity" => Order::Activity,
            "creation" => Order::Creation,
            "index" | "key" => Order::Index,
            "modifier" => Order::Modifier,
            "name" | "title" => Order::Name,
            "order" => Order::Natural,
            "size" => Order::Size,
            "z" => Order::Z,
            _ => return None,
        })
    }
}

/// A command's sort, checked.
#[derive(Debug, Clone, Copy)]
pub struct Sort {
    order: Option<Order>,
    reversed: bool,
}

impl Sort {
    /// The flags as given; a `-O` that is no order is tmux's `invalid sort
    /// order`.
    pub fn new(flags: &SortFlags) -> Result<Sort, String> {
        let order = match flags.order.as_deref() {
            None => None,
            Some(name) => Some(Order::from_name(name).ok_or("invalid sort order")?),
        };
        Ok(Sort {
            order,
            reversed: flags.reversed,
        })
    }

    /// As [`Sort::new`] for the commands that take a bad `-O` as no sort
    /// (`list-panes` checks it once, up front, and sorts each window).
    pub fn lenient(flags: &SortFlags) -> Sort {
        Sort {
            order: flags.order.as_deref().and_then(Order::from_name),
            reversed: flags.reversed,
        }
    }

    /// The order asked for, `None` for none.
    pub fn order(&self) -> Option<Order> {
        self.order
    }

    /// Whether `-r` was given.
    pub fn reversed(&self) -> bool {
        self.reversed
    }

    /// Sorts `items`; `cmp` compares two of them by an order other than
    /// [`Order::Natural`], its ties already settled.
    pub fn apply<T>(&self, items: &mut [T], cmp: impl Fn(Order, &T, &T) -> Ordering) {
        match self.order {
            None => {}
            Some(Order::Natural) => {
                if self.reversed {
                    items.reverse();
                }
            }
            Some(order) => items.sort_by(|a, b| {
                let result = cmp(order, a, b);
                if self.reversed {
                    result.reverse()
                } else {
                    result
                }
            }),
        }
    }
}

/// glibc's `qsort` for a small array (`msort_with_tmp`): halves sorted,
/// then merged taking the left item when `cmp` says it is not greater. Used
/// where tmux's comparator is no total order (`list-keys -O`), so the
/// result has to come from the same algorithm to be the same.
pub fn merge_sort<T: Clone>(items: &mut [T], cmp: &impl Fn(&T, &T) -> i32) {
    let n = items.len();
    if n <= 1 {
        return;
    }
    let n1 = n / 2;
    let (left, right) = items.split_at_mut(n1);
    merge_sort(left, cmp);
    merge_sort(right, cmp);
    let mut merged: Vec<T> = Vec::with_capacity(n);
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if cmp(&left[i], &right[j]) <= 0 {
            merged.push(left[i].clone());
            i += 1;
        } else {
            merged.push(right[j].clone());
            j += 1;
        }
    }
    merged.extend_from_slice(&left[i..]);
    merged.extend_from_slice(&right[j..]);
    items.clone_from_slice(&merged);
}

/// `strcmp` on names: by bytes.
pub fn by_name(a: &str, b: &str) -> Ordering {
    a.as_bytes().cmp(b.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(order: Option<&str>, reversed: bool) -> SortFlags {
        SortFlags {
            order: order.map(str::to_string),
            reversed,
        }
    }

    #[test]
    fn orders_are_tmux_s_names() {
        for name in [
            "Activity", "creation", "INDEX", "key", "modifier", "name", "title", "order", "size",
            "z",
        ] {
            assert!(Sort::new(&flags(Some(name), false)).is_ok(), "{name}");
        }
        assert_eq!(
            Sort::new(&flags(Some("nope"), false)).unwrap_err(),
            "invalid sort order"
        );
        assert!(Sort::lenient(&flags(Some("nope"), true)).order.is_none());
    }

    #[test]
    fn reverse_alone_does_nothing_and_with_order_reverses_the_list() {
        let mut list = vec![3, 1, 2];
        Sort::new(&flags(None, true))
            .unwrap()
            .apply(&mut list, |_, a, b| a.cmp(b));
        assert_eq!(list, [3, 1, 2]);
        Sort::new(&flags(Some("order"), true))
            .unwrap()
            .apply(&mut list, |_, a, b| a.cmp(b));
        assert_eq!(list, [2, 1, 3]);
        Sort::new(&flags(Some("index"), false))
            .unwrap()
            .apply(&mut list, |_, a, b| a.cmp(b));
        assert_eq!(list, [1, 2, 3]);
        Sort::new(&flags(Some("index"), true))
            .unwrap()
            .apply(&mut list, |_, a, b| a.cmp(b));
        assert_eq!(list, [3, 2, 1]);
    }
}
