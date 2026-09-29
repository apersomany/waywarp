// Lowercases and keeps only letters and digits, so "JP-Tokyo 3" matches "jptokyo3" and
// "São Paulo" matches "são paulo".
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
