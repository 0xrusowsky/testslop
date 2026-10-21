pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn classify(x: i32) -> &'static str {
    if x < 0 { "neg" } else { "pos" }
}

pub struct Point {
    pub x: u32,
    pub y: u32,
    pub z: u32,
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
        let points = vec![
            Point { x: 1, y: 2, z: 3 },
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
            Point { x: 1, y: 2, z: 3 },
        ];
        let total = points
            .iter()
            .map(|p| double(p.x))
            .sum::<u32>();
        let label = classify(total as i32);
        assert_eq!(label, "pos", "total {total} of {} points", points.len());
    }
}
