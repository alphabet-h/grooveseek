//! Packing generated Office Open XML parts into a zip, the container `.docx` / `.xlsx` /
//! `.pptx` files are.
//!
//! Shared by the retrieval tests that build their binary documents at run time. Other tests
//! that pack their own zips keep their copies, because existing tests are not edited.

use std::io::Write as _;

/// A zip holding each `(name, body)` of `parts` as a deflated entry, in order.
pub fn zip_parts(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::zip_parts;
    use std::io::Read as _;

    #[test]
    fn zip_parts_round_trips_names_and_bodies_in_order() {
        let bytes = zip_parts(&[("a.xml", "<a/>"), ("word/b.xml", "本文")]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 2);
        let mut read = |i: usize| {
            let mut f = archive.by_index(i).unwrap();
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            (f.name().unwrap().to_string(), s)
        };
        assert_eq!(read(0), ("a.xml".to_string(), "<a/>".to_string()));
        assert_eq!(read(1), ("word/b.xml".to_string(), "本文".to_string()));
    }
}
