//! CSV with leading/trailing commas; a dead PID exits 0 with the header only (no data, not zero).

use super::ParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteCounters {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// `Ok(None)` = header only (process has no counters, e.g. it is gone).
pub fn parse_nettop(stdout: &str) -> Result<Option<ByteCounters>, ParseError> {
    let mut lines = stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty());
    let header = lines
        .next()
        .ok_or_else(|| ParseError::new("nettop", "empty output"))?;
    let cols: Vec<&str> = header.split(',').map(str::trim).collect();
    let col = |name: &str| {
        cols.iter()
            .position(|c| *c == name)
            .ok_or_else(|| ParseError::new("nettop", format!("header lacks {name}: {header:?}")))
    };
    let (idx_in, idx_out) = (col("bytes_in")?, col("bytes_out")?);
    let Some(line) = lines.next() else {
        return Ok(None);
    };
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    if fields.len() <= idx_in.max(idx_out) {
        return Err(ParseError::new(
            "nettop",
            format!("truncated data row {line:?}"),
        ));
    }
    let num = |i: usize| {
        fields[i]
            .parse::<u64>()
            .map_err(|_| ParseError::new("nettop", format!("bad counter in {line:?}")))
    };
    Ok(Some(ByteCounters {
        bytes_in: num(idx_in)?,
        bytes_out: num(idx_out)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = include_str!("../../tests/fixtures/nettop_ok.csv");
    const DEAD: &str = include_str!("../../tests/fixtures/nettop_dead_pid.csv");

    #[test]
    fn parses_counters_of_the_master() {
        assert_eq!(
            parse_nettop(OK).unwrap(),
            Some(ByteCounters {
                bytes_in: 662_459,
                bytes_out: 355_897
            })
        );
    }

    #[test]
    fn header_only_is_no_data_not_zero() {
        assert_eq!(parse_nettop(DEAD).unwrap(), None);
    }

    #[test]
    fn empty_or_foreign_output_is_an_error() {
        assert_eq!(parse_nettop("").unwrap_err().probe, "nettop");
        assert!(parse_nettop("nettop: invalid option\n").is_err());
        assert!(parse_nettop(",bytes_in,bytes_out,\nssh.1,abc,2,\n").is_err());
        assert!(
            parse_nettop(",bytes_in,bytes_out,\nssh.1,5\n").is_err(),
            "truncated row is not no-data"
        );
    }

    #[test]
    fn column_order_comes_from_the_header() {
        let out = ",bytes_out,bytes_in,\r\nssh.7,10,20,\r\n";
        assert_eq!(
            parse_nettop(out).unwrap(),
            Some(ByteCounters {
                bytes_in: 20,
                bytes_out: 10
            })
        );
    }
}
