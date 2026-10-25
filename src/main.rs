fn main() -> anyhow::Result<()> {
    let paths = agentmail::paths::Paths::from_env()?;
    println!("agentmail: data in {}", paths.data_dir.display());
    Ok(())
}
