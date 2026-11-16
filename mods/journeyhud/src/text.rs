//! Text at a font size of its own, in the foreground. The overlay's texts (the numbers in the
//! bars, the compass letters, the clock) each have a size that follows their own setting, so
//! none of them goes through the global font scale: changing one setting does not resize the
//! text of another.

use hudhook::imgui::{Ui, sys};

/// The size of `text` at a font size of `em` pixels.
pub fn measure(ui: &Ui, text: &str, em: f32) -> [f32; 2] {
    let ratio = em / ui.current_font_size();
    ui.calc_text_size(text).map(|side| side * ratio)
}

/// `text` with its top left at `pos`, at a font size of `em` pixels, `color` as RGBA 0 to 1.
pub fn add(text: &str, [x, y]: [f32; 2], em: f32, color: [f32; 4]) {
    let range = text.as_bytes().as_ptr_range();
    // SAFETY: called while a frame is being built, so the foreground draw list and the font
    // exist; the text range is valid for the call, and ImGui copies what it draws.
    unsafe {
        sys::ImDrawList_AddText_FontPtr(
            sys::igGetForegroundDrawList(),
            sys::igGetFont(),
            em,
            sys::ImVec2 { x, y },
            pack(color),
            range.start as *const std::os::raw::c_char,
            range.end as *const std::os::raw::c_char,
            0.0,
            std::ptr::null(),
        );
    }
}

/// `text` as [add] draws it, over a copy of it in `shadow` one `offset` pixels down and right.
pub fn add_shadowed(
    text: &str,
    pos: [f32; 2],
    em: f32,
    color: [f32; 4],
    shadow: [f32; 4],
    offset: f32,
) {
    add(text, [pos[0] + offset, pos[1] + offset], em, shadow);
    add(text, pos, em, color);
}

/// [add_shadowed] with the color depending on the height within the text: `color_at` gets 0 at the
/// bottom of the glyphs and 1 at the top.
pub fn add_shadowed_by_height(
    text: &str,
    pos: [f32; 2],
    em: f32,
    color_at: impl Fn(f32) -> [f32; 4],
    shadow: [f32; 4],
    offset: f32,
) {
    add(text, [pos[0] + offset, pos[1] + offset], em, shadow);
    // SAFETY: called while a frame is being built; the vertices read and written are the ones
    // `add` has just appended to the draw list's buffer.
    unsafe {
        let list = sys::igGetForegroundDrawList();
        let first = (*list).VtxBuffer.Size as usize;
        add(text, pos, em, [1.0; 4]);
        let end = (*list).VtxBuffer.Size as usize;
        let vertices =
            std::slice::from_raw_parts_mut((*list).VtxBuffer.Data.add(first), end - first);
        let (top, bottom) = vertices
            .iter()
            .fold((f32::MAX, f32::MIN), |(top, bottom), v| (top.min(v.pos.y), bottom.max(v.pos.y)));
        let span = (bottom - top).max(1.0);
        for vertex in vertices {
            vertex.col = pack(color_at(1.0 - (vertex.pos.y - top) / span));
        }
    }
}

/// ImGui's packed color: red in the low byte, alpha in the high one.
fn pack([r, g, b, a]: [f32; 4]) -> u32 {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    byte(r) | byte(g) << 8 | byte(b) << 16 | byte(a) << 24
}

#[cfg(test)]
mod tests {
    use super::pack;

    #[test]
    fn a_color_is_packed_with_red_low_and_alpha_high() {
        assert_eq!(pack([1.0, 0.0, 0.0, 0.0]), 0x0000_00FF);
        assert_eq!(pack([0.0, 0.0, 1.0, 1.0]), 0xFFFF_0000);
        assert_eq!(pack([2.0, -1.0, 0.5, 1.0]), 0xFF80_00FF);
    }
}
