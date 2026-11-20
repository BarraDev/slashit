//! A card menu opened from the keyboard opens beside its button, and a mouse
//! click still opens it at the pointer. Both are checked at the wide and
//! narrow widths the board is reviewed at. Keyboard activation reports no
//! pointer position, so the menu's anchor must come from the button itself.

use super::*;

const WIDTHS: [(&str, (u32, u32)); 2] = [("wide", (1280, 800)), ("narrow", (900, 760))];
const MENU_BUTTON: &str = "[data-testid=\"task-card-menu\"]";
const MENU_ITEM: &str = "[data-testid=\"task-menu-move-to\"]";
/// The component keeps the menu inside the viewport with these margins.
const MENU_MARGIN_WIDTH: f64 = 200.0;
const MENU_MARGIN_HEIGHT: f64 = 300.0;
/// The menu's left and top are whole pixels taken from the pointer, so a mouse menu must match exactly.
const TOLERANCE: f64 = 1.0;
/// The keyboard anchor is read from the button after the activation. The button can move by a few
/// pixels in between (the pointer may rest over the card), so this allows that drift. The defect
/// placed the menu about 15 pixels away, so it still fails.
const KEYBOARD_TOLERANCE: f64 = 6.0;

#[tokio::test(flavor = "multi_thread")]
async fn a_card_menu_opened_from_the_keyboard_sits_beside_its_button() {
    let context = TestContext::new("kanban_card_menu").expect("harness setup");
    let outcome = card_menu_journey(&context).await;
    context.finish(outcome);
}

async fn card_menu_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("kanban-card-menu").await?;
    let outcome = card_menu_checks(session.driver(), &repository).await;
    context.close_session(session, "kanban-card-menu", &outcome).await?;
    outcome
}

async fn card_menu_checks(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, .. } = create_prerequisites(
        driver,
        repository,
        "Card menu target",
        "Opened from the keyboard and from the mouse.",
    )
    .await?;
    open_board(driver, &project_id).await?;
    await_menu_button(driver).await?;

    for (name, (width, height)) in WIDTHS {
        driver
            .set_window_rect(0, 0, width, height)
            .await
            .context("could not size the window")?;

        // Keyboard: focus the button and press Enter, as a keyboard user does.
        focus_menu_button(driver).await?;
        press(driver, Key::Enter).await?;
        ui::visible(driver, MENU_ITEM)
            .await
            .with_context(|| format!("{name}: the keyboard did not open the card menu"))?;
        let geometry = menu_geometry(driver).await?;
        let (x, y) = clamped(geometry.button_left, geometry.button_bottom, &geometry);
        check_position(&geometry, (x, y), KEYBOARD_TOLERANCE, &format!("{name}, keyboard"))?;
        assert!(
            geometry.menu_left > TOLERANCE || geometry.menu_top > TOLERANCE,
            "{name}: the keyboard menu opened at the viewport corner: {geometry:?}"
        );

        press(driver, Key::Escape).await?;
        await_menu_closed(driver, name).await?;

        // Mouse: a pointer click on the button keeps the pointer placement.
        let (x, y) = button_center(driver).await?;
        driver
            .action_chain()
            .move_to(x, y)
            .click()
            .perform()
            .await
            .context("could not click the card menu button")?;
        ui::visible(driver, MENU_ITEM)
            .await
            .with_context(|| format!("{name}: the mouse did not open the card menu"))?;
        let geometry = menu_geometry(driver).await?;
        // The menu must sit where the pointer actually pressed, which the page recorded, and that
        // point must be on the button.
        let (pointer_x, pointer_y) = geometry
            .pointer
            .with_context(|| format!("{name}: the page recorded no pointer press"))?;
        let (on_x, on_y) = (geometry.button_center.0, geometry.button_center.1);
        if (pointer_x - on_x).abs() > geometry.button_size.0 / 2.0
            || (pointer_y - on_y).abs() > geometry.button_size.1 / 2.0
        {
            bail!("{name}: the pointer pressed at ({pointer_x}, {pointer_y}), away from the button centre ({on_x}, {on_y})");
        }
        let (x, y) = clamped(pointer_x, pointer_y, &geometry);
        check_position(&geometry, (x, y), TOLERANCE, &format!("{name}, mouse"))?;

        press(driver, Key::Escape).await?;
        await_menu_closed(driver, name).await?;
    }
    Ok(())
}

/// Wait for the card's menu button to exist. It is `opacity: 0` until it is hovered or
/// focused, so a displayed check would not accept it while it is still idle.
async fn await_menu_button(driver: &WebDriver) -> Result<WebElement> {
    driver
        .query(By::Css(MENU_BUTTON))
        .wait(Duration::from_secs(20), Duration::from_millis(200))
        .first()
        .await
        .context("the card menu button is not shown")
}

/// The viewport point at the centre of the card's menu button, after bringing it into view.
async fn button_center(driver: &WebDriver) -> Result<(i64, i64)> {
    let raw = page(
        driver,
        r#"
        // Record the pointer press the mouse click makes, from the page itself, so the
        // check compares the menu with where the pointer really was.
        if (!window.__slashitRecorder) {
            window.__slashitRecorder = true;
            document.addEventListener("mousedown", (e) => {
                window.__slashitPointer = [e.clientX, e.clientY];
            }, true);
        }
        window.__slashitPointer = null;
        const button = document.querySelector(arguments[0]);
        button.scrollIntoView({ block: "center", inline: "center" });
        const r = button.getBoundingClientRect();
        return [r.left + r.width / 2, r.top + r.height / 2];
        "#,
        vec![Value::String(MENU_BUTTON.to_string())],
    )
    .await?;
    let point = raw.as_array().context("the button has no centre point")?;
    let coordinate = |index: usize| -> Result<i64> {
        Ok(point
            .get(index)
            .and_then(Value::as_f64)
            .with_context(|| format!("the button centre has no coordinate {index}: {raw}"))?
            as i64)
    };
    Ok((coordinate(0)?, coordinate(1)?))
}

/// Focus the card's menu button without clicking it, so the next key press
/// is the keyboard activation under test.
async fn focus_menu_button(driver: &WebDriver) -> Result<()> {
    page(
        driver,
        "document.querySelector(arguments[0]).focus(); return true;",
        vec![Value::String(MENU_BUTTON.to_string())],
    )
    .await?;
    Ok(())
}

async fn press(driver: &WebDriver, key: Key) -> Result<()> {
    driver
        .active_element()
        .await?
        .send_keys(key)
        .await
        .context("could not send a key to the focused element")
}

/// Where the component clamps a menu anchor to, as it computes it.
fn clamped(x: f64, y: f64, geometry: &MenuGeometry) -> (f64, f64) {
    (
        x.trunc().min(geometry.viewport_width - MENU_MARGIN_WIDTH).max(0.0),
        y.trunc().min(geometry.viewport_height - MENU_MARGIN_HEIGHT).max(0.0),
    )
}

fn check_position(geometry: &MenuGeometry, expected: (f64, f64), tolerance: f64, label: &str) -> Result<()> {
    let close = (geometry.menu_left - expected.0).abs() <= tolerance
        && (geometry.menu_top - expected.1).abs() <= tolerance;
    if close {
        Ok(())
    } else {
        bail!(
            "{label}: the menu is at ({}, {}), expected ({}, {}): {geometry:?}",
            geometry.menu_left,
            geometry.menu_top,
            expected.0,
            expected.1
        )
    }
}

async fn await_menu_closed(driver: &WebDriver, name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let open = page(
            driver,
            "return document.querySelector('[data-testid=\"task-menu-move-to\"]') !== null;",
            vec![],
        )
        .await?;
        if open != Value::Bool(true) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("{name}: the card menu did not close on Escape");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Debug)]
struct MenuGeometry {
    button_left: f64,
    button_bottom: f64,
    button_center: (f64, f64),
    button_size: (f64, f64),
    /// The client coordinates of the last `mousedown` the page saw, if any.
    pointer: Option<(f64, f64)>,
    menu_left: f64,
    menu_top: f64,
    viewport_width: f64,
    viewport_height: f64,
}

async fn menu_geometry(driver: &WebDriver) -> Result<MenuGeometry> {
    let raw = page(
        driver,
        r#"
        const button = document.querySelector('[data-testid="task-card-menu"]').getBoundingClientRect();
        const menu = document
            .querySelector('[data-testid="task-menu-move-to"]')
            .closest('.fixed.z-50')
            .getBoundingClientRect();
        const pointer = window.__slashitPointer || null;
        return {
            buttonLeft: button.left,
            buttonBottom: button.bottom,
            centerX: button.left + button.width / 2,
            centerY: button.top + button.height / 2,
            width: button.width,
            height: button.height,
            pointerX: pointer ? pointer[0] : null,
            pointerY: pointer ? pointer[1] : null,
            menuLeft: menu.left,
            menuTop: menu.top,
            viewportWidth: window.innerWidth,
            viewportHeight: window.innerHeight,
        };
        "#,
        vec![],
    )
    .await?;
    let number = |key: &str| -> Result<f64> {
        raw.get(key)
            .and_then(Value::as_f64)
            .with_context(|| format!("the menu geometry has no {key}: {raw}"))
    };
    let pointer = match (raw.get("pointerX").and_then(Value::as_f64), raw.get("pointerY").and_then(Value::as_f64)) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    Ok(MenuGeometry {
        button_left: number("buttonLeft")?,
        button_bottom: number("buttonBottom")?,
        button_center: (number("centerX")?, number("centerY")?),
        button_size: (number("width")?, number("height")?),
        pointer,
        menu_left: number("menuLeft")?,
        menu_top: number("menuTop")?,
        viewport_width: number("viewportWidth")?,
        viewport_height: number("viewportHeight")?,
    })
}
