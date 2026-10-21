pub fn unused_helper() -> u32 {
    7
}

pub fn sample() -> u32 {
    21
}

pub struct FakeStore {
    pub value: u32,
}

impl FakeStore {
    pub fn get(&self) -> u32 {
        self.value
    }
}

mod tests {
    #[test]
    fn sample_is_21() {
        assert_eq!(super::sample(), 21);
        assert_eq!(super::FakeStore { value: 1 }.get(), 1);
    }
}
