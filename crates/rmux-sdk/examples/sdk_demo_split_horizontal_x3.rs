//! Three down-splits → 4 panes stacked.

use rmux_sdk::SplitDirection;

#[path = "sdk_demo_helpers/mod.rs"]
mod sdk_demo_helpers;

#[tokio::main]
async fn main() -> rmux_sdk::Result<()> {
    let (_rmux, session) = sdk_demo_helpers::demo_session("splithx3").await?;
    sdk_demo_helpers::paint_idle_prompt(&session).await?;

    // example:start
    let pane = session.pane(0, 0);
    pane.split(SplitDirection::Down).await?;
    pane.split(SplitDirection::Down).await?;
    pane.split(SplitDirection::Down).await?;
    // example:end

    let panes = session.window(0).panes().await?;
    println!("visible panes: {}", panes.len());
    sdk_demo_helpers::cleanup(session).await
}
