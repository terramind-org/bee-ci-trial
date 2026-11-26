//! Conflict markers for working copies. The server never writes them (SPEC §7.3.2).

/// How a conflict block is rendered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MarkerStyle {
    /// Ours, base and theirs sections (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`).
    #[default]
    Diff3,
    /// Ours and theirs sections only (`<<<<<<<`, `=======`, `>>>>>>>`).
    Merge,
}

const OURS: &[u8] = b"<<<<<<< ours\n";
const BASE: &[u8] = b"||||||| base\n";
const SEPARATOR: &[u8] = b"=======\n";
const THEIRS: &[u8] = b">>>>>>> theirs\n";

/// Appends one conflict block to `out`.
///
/// The block is, line by line: `<<<<<<< ours`, the ours lines, `||||||| base` and the base
/// lines (only in [`MarkerStyle::Diff3`]), `=======`, the theirs lines, `>>>>>>> theirs`.
/// Every marker line ends in `\n`. Before each marker, if `out` is non-empty and does not end
/// in `\n`, a single `\n` is inserted so the marker starts a line; this happens when a section
/// ends with a final line that has no terminator.
pub fn render_conflict(
    out: &mut Vec<u8>,
    ours: &[&[u8]],
    base: &[&[u8]],
    theirs: &[&[u8]],
    style: MarkerStyle,
) {
    marker(out, OURS);
    ours.iter().for_each(|l| out.extend_from_slice(l));
    if style == MarkerStyle::Diff3 {
        marker(out, BASE);
        base.iter().for_each(|l| out.extend_from_slice(l));
    }
    marker(out, SEPARATOR);
    theirs.iter().for_each(|l| out.extend_from_slice(l));
    marker(out, THEIRS);
}

fn marker(out: &mut Vec<u8>, line: &[u8]) {
    if out.last().is_some_and(|&b| b != b'\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff3_block() {
        let mut out = b"x\n".to_vec();
        render_conflict(
            &mut out,
            &[b"o\n"],
            &[b"b\n"],
            &[b"t\n"],
            MarkerStyle::Diff3,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "x\n<<<<<<< ours\no\n||||||| base\nb\n=======\nt\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn newline_inserted_after_unterminated_sections() {
        let mut out = Vec::new();
        render_conflict(&mut out, &[b"o"], &[b"b"], &[b"t"], MarkerStyle::Merge);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<<<<<<< ours\no\n=======\nt\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn empty_sections() {
        let mut out = Vec::new();
        render_conflict(&mut out, &[], &[], &[b"t\n"], MarkerStyle::Diff3);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<<<<<<< ours\n||||||| base\n=======\nt\n>>>>>>> theirs\n"
        );
    }
}
