pub mod inspect;
pub mod manage;
pub mod run;

use model_router::config::Role;

pub fn parse_role(text: &str) -> std::result::Result<Role, String> {
    Role::ALL
        .into_iter()
        .find(|role| role.label() == text)
        .ok_or_else(|| {
            let names: Vec<_> = Role::ALL.iter().map(|r| r.label()).collect();
            format!("expected one of {}", names.join(", "))
        })
}

pub fn parse_rate(text: &str) -> std::result::Result<f64, String> {
    let rate: f64 = text.parse().map_err(|_| "expected a number".to_owned())?;
    if (0.0..=1.0).contains(&rate) {
        Ok(rate)
    } else {
        Err("expected a fraction from 0 to 1, e.g. 0.8".into())
    }
}
