use rmux_sdk as _;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_scaffold_compiles() {
        assert_eq!(core::any::type_name::<()>(), "()");
    }
}
