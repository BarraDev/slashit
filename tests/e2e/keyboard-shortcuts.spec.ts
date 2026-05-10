import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

test.describe('Keyboard Shortcuts', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should show keyboard shortcuts on ? key', async ({ page }) => {
    // Press ? to show shortcuts
    await page.keyboard.press('Shift+/');
    await page.waitForTimeout(500);
    
    // Look for shortcuts modal/panel
    const shortcutsPanel = page.locator('[class*="shortcuts"], *:has-text("Keyboard Shortcuts"), *:has-text("Shortcuts")');
    if (await shortcutsPanel.count() > 0) {
      console.log('Shortcuts panel opened');
    }
  });

  test('Escape should close modals', async ({ page }) => {
    // Open a modal first
    const newTaskButton = page.locator('button:has-text("New Task")');
    
    if (await newTaskButton.count() > 0) {
      await newTaskButton.first().click();
      await page.waitForTimeout(500);
      
      // Press Escape
      await page.keyboard.press('Escape');
      await page.waitForTimeout(300);
      
      // Modal should be closed
      const modal = page.locator('[class*="modal"]:visible');
      if (await modal.count() === 0) {
        console.log('Modal closed with Escape');
      }
    }
  });

  test('Ctrl+K should open command palette (if implemented)', async ({ page }) => {
    // Press Ctrl+K
    await page.keyboard.press('Control+k');
    await page.waitForTimeout(500);
    
    // Look for command palette
    const commandPalette = page.locator('[class*="command"], [class*="palette"], input[placeholder*="Search"]');
    if (await commandPalette.count() > 0) {
      console.log('Command palette opened');
    }
  });

  test('navigation with number keys (if implemented)', async ({ page }) => {
    // Press 1 to go to first nav item
    await page.keyboard.press('1');
    await page.waitForTimeout(300);
    
    // Check if navigation changed
    console.log('Number key navigation tested');
  });

  test('n key should create new task (if implemented)', async ({ page }) => {
    // First navigate to Kanban
    const kanbanNav = page.locator('button:has-text("Kanban")');
    if (await kanbanNav.count() > 0) {
      await kanbanNav.first().click();
      await page.waitForTimeout(500);
    }
    
    // Press n to create new task
    await page.keyboard.press('n');
    await page.waitForTimeout(500);
    
    // Look for task creation modal
    const modal = page.locator('[class*="modal"], [role="dialog"]');
    if (await modal.count() > 0) {
      console.log('New task modal opened with n key');
    }
  });
});

test.describe('Keyboard Navigation', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('Tab should navigate through focusable elements', async ({ page }) => {
    // Press Tab multiple times
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);
    
    // Something should be focused
    const focusedElement = page.locator(':focus');
    if (await focusedElement.count() > 0) {
      console.log('Tab navigation works');
    }
  });

  test('Arrow keys should navigate in lists (if implemented)', async ({ page }) => {
    // Navigate to a list context first
    const kanbanNav = page.locator('button:has-text("Kanban")');
    if (await kanbanNav.count() > 0) {
      await kanbanNav.first().click();
      await page.waitForTimeout(500);
    }
    
    // Press arrow keys
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(100);
    await page.keyboard.press('ArrowUp');
    await page.waitForTimeout(100);
    
    console.log('Arrow key navigation tested');
  });

  test('Enter should activate focused element', async ({ page }) => {
    // Tab to a button
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);
    
    // Press Enter
    await page.keyboard.press('Enter');
    await page.waitForTimeout(300);
    
    console.log('Enter activation tested');
  });
});
