fn triple(x: u32) -> u32 {
    x * 3
}

fn main() {
    println!("{}", triple(2));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_two() {
        assert_eq!(triple(2), 6);
    }

    #[test]
    fn triple_two_again() {
        assert_eq!(triple(2), 6);
    }
}
