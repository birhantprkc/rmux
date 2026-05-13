//! Variant of `sdk_demo_split_window` that performs TWO splits in a row,
//! producing 3 panes side-by-side. Used by the multi-run feature of the
//! SDK demo.

use rmux_sdk::SplitDirection;

#[path = "sdk_demo_helpers/mod.rs"]
mod sdk_demo_helpers;

#[tokio::main]
async fn main() -> rmux_sdk::Result<()> {
    let (_rmux, session) = sdk_demo_helpers::demo_session("splitx2").await?;
    sdk_demo_helpers::paint_idle_prompt(&session).await?;

    // example:start
    let pane = session.pane(0, 0);
    pane.split(SplitDirection::Right).await?;
    pane.split(SplitDirection::Right).await?;
    // example:end

    let panes = session.window(0).panes().await?;
    println!("visible panes: {}", panes.len());
    sdk_demo_helpers::cleanup(session).await
}
