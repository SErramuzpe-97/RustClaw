#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(not(feature = "telegram"))]
pub mod telegram {
    use crate::agent::Agent;
    use anyhow::{Result, bail};
    pub async fn run(_: Agent) -> Result<()> {
        bail!("this build was compiled without the `telegram` feature")
    }
}
