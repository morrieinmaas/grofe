#![no_main]
//! The sixel parser reads what a pane's program sends: no panic, and a
//! parsed image can be scaled and printed.

use grofe::server::sixel::Sixel;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u8, u8, u8, &[u8])| {
    let (p2, xpixel, ypixel, body) = input;
    let (xpixel, ypixel) = (u32::from(xpixel), u32::from(ypixel));
    if let Some(image) = Sixel::parse(body, u32::from(p2 % 3), xpixel, ypixel) {
        let _ = image.size_px();
        let _ = image.size_in_cells();
        let _ = image.print(None);
    }
});
