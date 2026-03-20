pub fn hello() -> &'static str {
    "Hello from galoy-agents!"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_returns_greeting() {
        assert_eq!(hello(), "Hello from galoy-agents!");
    }
}
