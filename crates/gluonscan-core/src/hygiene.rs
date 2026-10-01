//! Token display hygiene: detect and strip the Unicode control characters a spoofed token uses to
//! disguise its name or symbol (e.g. a right-to-left override that renders "CDSU" as "USDC"). A
//! wallet reader runs [`is_spoofed_token`] to drop the token and [`sanitize_display`] so a survivor
//! can never render a hidden symbol. Pure string logic — no allocation beyond the sanitized output.

/// Characters that can disguise a token's display string: bidirectional overrides/isolates,
/// directionality marks, zero-width joiners/spaces, the BOM, and any other control character.
fn is_disguise_control(c: char) -> bool {
    matches!(c,
        '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
        | '\u{200E}' | '\u{200F}' // LRM, RLM
        | '\u{061C}'              // Arabic letter mark
        | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' // ZWSP, ZWNJ, ZWJ, BOM
    ) || c.is_control()
}

/// Whether `text` contains any disguise/control character.
pub fn has_disguise_control(text: &str) -> bool {
    text.chars().any(is_disguise_control)
}

/// Whether a token's `name` or `symbol` carries a disguise/control character — a spoof signal.
pub fn is_spoofed_token(name: &str, symbol: &str) -> bool {
    has_disguise_control(name) || has_disguise_control(symbol)
}

/// Strip every disguise/control character from `text`, keeping all other characters unchanged.
pub fn sanitize_display(text: &str) -> String {
    text.chars().filter(|c| !is_disguise_control(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_spoofed() {
        assert!(!is_spoofed_token("USD Coin", "USDC"));
        assert_eq!(sanitize_display("USDC"), "USDC");
    }

    #[test]
    fn rlo_disguise_is_detected_and_stripped() {
        // "USD\u{202E}C" uses a right-to-left override to disguise the symbol.
        let sym = "USD\u{202E}C";
        assert!(is_spoofed_token("", sym));
        assert_eq!(sanitize_display(sym), "USDC");
    }

    #[test]
    fn zero_width_and_bom_are_detected() {
        assert!(is_spoofed_token("Te\u{200B}st", ""));
        assert!(is_spoofed_token("", "\u{FEFF}SOL"));
        assert_eq!(sanitize_display("\u{FEFF}SOL"), "SOL");
    }

    #[test]
    fn control_chars_are_detected() {
        assert!(is_spoofed_token("line\nbreak", ""));
        assert_eq!(sanitize_display("a\tb"), "ab");
    }
}
