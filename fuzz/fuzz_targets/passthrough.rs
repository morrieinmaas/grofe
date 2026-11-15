#![no_main]
//! The passthrough/APC scanner reads a pane's raw output: no panic, however
//! the bytes are split across reads, and with no `ESC` in the input the
//! bytes come back unchanged. Splitting the input must find the same events
//! as scanning it whole.

use grofe::server::passthrough::Scanner;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (bool, u8, &[u8])| {
    let (sixel, cut, data) = input;
    let cut = usize::from(cut).min(data.len());

    let mut whole = Scanner::default();
    whole.set_sixel(sixel);
    let (out_whole, events_whole) = whole.scan(data);
    let (out_whole, events_whole) = (out_whole.into_owned(), events_whole.len());

    let mut split = Scanner::default();
    split.set_sixel(sixel);
    let (a, ev_a) = split.scan(&data[..cut]);
    let mut out_split = a.into_owned();
    let mut events_split = ev_a.len();
    let (b, ev_b) = split.scan(&data[cut..]);
    out_split.extend_from_slice(&b);
    events_split += ev_b.len();

    if !data.contains(&0x1b) {
        assert_eq!(out_whole, data);
    }
    assert_eq!(events_split, events_whole, "events differ when split at {cut}");
});
