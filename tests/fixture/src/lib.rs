pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn classify(x: i32) -> &'static str {
    if x < 0 { "neg" } else { "pos" }
}

#[derive(Debug)]
pub enum ParseError {
    NotANumber(String),
}

pub fn parse(s: &str) -> Result<u32, ParseError> {
    s.parse().map_err(|_| ParseError::NotANumber(s.to_owned()))
}

pub struct Point {
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

#[cfg(test)]
impl Point {
    fn origin() -> Self {
        Point { x: 0, y: 0, z: 0 }
    }
}

#[cfg(test)]
pub mod test_util;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Config {
    pub n: u32,
}

#[derive(Clone, Debug)]
pub struct Celsius(pub i32);

impl PartialEq for Celsius {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

pub fn with_ctx<T>(f: impl FnOnce(&mut u32) -> T) -> T {
    let mut ctx = 0;
    f(&mut ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_two() {
        let input = 2;
        let out = double(input);
        assert_eq!(out, 4);
    }

    #[test]
    fn double_three() {
        let input = 3;
        let out = double(input);
        assert_eq!(out, 6);
    }

    #[test]
    fn double_two_again() {
        let input = 2;
        let out = double(input);
        assert_eq!(out, 4);
    }

    #[test]
    fn double_is_even() {
        assert!(double(5) % 2 == 0);
    }

    #[test]
    #[should_panic]
    fn double_overflow_panics() {
        double(u32::MAX);
    }

    #[test]
    fn double_smoke() {
        double(1);
    }

    #[test]
    fn double_is_deterministic() {
        assert_eq!(double(1), double(1));
    }

    #[test]
    fn classify_neg() {
        assert_eq!(classify(-1), "neg");
    }

    #[test]
    fn classify_both() {
        assert_eq!(classify(-1), "neg");
        assert_eq!(classify(1), "pos");
    }

    #[test]
    fn classify_pos_matches() {
        let "pos" = classify(1) else { panic!("expected pos") };
    }

    #[test]
    fn point_fields() {
        let p = Point { x: 1, y: 2, z: 3 };
        assert_eq!(p.x, 1);
        assert_eq!(p.y, 2);
        assert_eq!(p.z, 3);
    }

    fn check_double(x: u32) {
        assert_eq!(double(x), x + x);
    }

    #[test]
    fn double_via_helper() {
        check_double(7);
    }

    fn halves_back(x: u32) {
        assert_eq!(double(x) / 2, x);
    }

    #[test]
    fn double_halves() {
        halves_back(9);
    }

    #[test]
    fn parse_rejects_words() {
        let result = parse("seven");
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(ParseError::NotANumber(_))
        ));
    }

    #[test]
    fn parse_rejects_empty() {
        let result = parse("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_sign() {
        let input = "-";
        let result = parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ok() {
        let input = "7";
        assert!(parse(input).is_ok());
        assert_eq!(parse(input).unwrap(), 7);
    }

    fn seven() -> &'static str {
        "7"
    }

    #[test]
    fn parse_ok_via_helper() {
        let input = seven();
        assert!(parse(input).is_ok());
    }

    const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/render.golden");

    fn check_golden(actual: &str) {
        if std::env::var("DEMO_UPDATE_GOLDEN").is_ok() {
            std::fs::write(GOLDEN, actual).unwrap();
        }
        assert_eq!(std::fs::read_to_string(GOLDEN).unwrap().trim(), actual);
    }

    #[test]
    fn render_matches_golden() {
        check_golden(&double(21).to_string());
    }

    struct FakeStore {
        value: u32,
    }

    impl FakeStore {
        fn get(&self) -> u32 {
            self.value
        }
    }

    #[test]
    fn fake_store_reads_origin() {
        let p = Point::origin();
        assert_eq!(FakeStore { value: p.x }.get(), 0);
    }

    #[test]
    fn needs_tool() {
        if std::env::var("DEMO_TOOL").is_err() {
            eprintln!("skipping: DEMO_TOOL is not set");
            return;
        }
        assert_eq!(double(1), 2);
    }

    #[test]
    #[ignore]
    fn ignored_without_reason() {
        assert_eq!(double(2), 4);
    }

    #[test]
    fn config_clone_eq() {
        let c = Config { n: 1 };
        assert_eq!(c.clone(), c);
    }

    #[test]
    fn celsius_compare() {
        let a = Celsius(1);
        let b = Celsius(1);
        assert_eq!(a, b);
    }

    #[test]
    fn std_vec_len() {
        assert_eq!(vec![1, 2].len(), 2);
    }

    #[test]
    fn loop_over_nothing() {
        let items: Vec<u32> = vec![];
        for i in &items {
            assert_eq!(double(*i), i * 2);
        }
        if let Ok(n) = parse("x") {
            assert_eq!(n, 0);
        }
    }

    // --- repeated blocks and shared setup ---

    #[test]
    fn double_repeated_block() {
        let a = double(1);
        let b = a + 1;
        let c = b * 2;
        assert_eq!(c, 6);
        let a = double(2);
        let b = a + 1;
        let c = b * 2;
        assert_eq!(c, 10);
        let a = double(3);
        let b = a + 1;
        let c = b * 2;
        assert_eq!(c, 14);
        let a = double(4);
        let b = a + 1;
        let c = b * 2;
        assert_eq!(c, 18);
    }

    #[test]
    fn total_is_positive() {
        let points = vec![
            Point { x: 1, y: 2, z: 3 },
        ];
        let total = points
            .iter()
            .map(|p| double(p.x))
            .sum::<u32>();
        assert!(total > 0);
    }

    #[test]
    fn total_is_even() {
        // Build the points
        let points = vec![
            Point { x: 2, y: 2, z: 3 },
        ];
        let total = points
            .iter()
            .map(|p| double(p.x))
            .sum::<u32>();
        if total % 2 != 0 {
            panic!("odd total {total}");
        }
    }

    #[test]
    fn total_matches_classify() {
        let points = vec![
            Point { x: 3, y: 2, z: 3 },
        ];
        let total = points
            .iter()
            .map(|p| double(p.x))
            .sum::<u32>();
        let label = classify(total as i32);
        assert_eq!(label, "pos", "total {total} of {} points", points.len());
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn ctx_one() {
        with_ctx(|ctx| {
            *ctx += 1;
            assert_eq!(*ctx, 1);
        })
    }

    #[test]
    fn ctx_double() {
        with_ctx(|ctx| {
            *ctx = double(4);
            assert_eq!(*ctx, 8);
        })
    }

    #[test]
    fn ctx_classify() {
        with_ctx(|ctx| {
            *ctx = 3;
            assert_eq!(classify(*ctx as i32), "pos");
        })
    }

    #[test]
    fn ctx_parse() {
        with_ctx(|ctx| {
            *ctx = parse("12").unwrap();
            assert_eq!(*ctx, 12);
        })
    }

    #[test]
    fn ctx_point() {
        with_ctx(|ctx| {
            let p = Point { x: *ctx, y: 1, z: 2 };
            assert_eq!(p.y + p.z, 3);
        })
    }
}
