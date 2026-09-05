use frona_model_catalog::CatalogSources;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("download"))
        || args.next().as_deref() != Some(std::ffi::OsStr::new("--output"))
    {
        return Err("usage: frona-model-catalog download --output DIRECTORY".into());
    }
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    CatalogSources::load(&output).download_all().await?;
    Ok(())
}
