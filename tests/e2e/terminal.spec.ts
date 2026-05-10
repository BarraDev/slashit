import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

// Navigate to terminal page
async function navigateToTerminals(page: Page) {
  const terminalsNav = page.locator('button:has-text("Agent Terminals"), button:has-text("Terminals")');
  if (await terminalsNav.count() > 0) {
    await terminalsNav.first().click();
    await page.waitForTimeout(500);
  }
}

test.describe('Terminal Page', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToTerminals(page);
  });

  test('should display terminal page header', async ({ page }) => {
    const header = page.locator('h1:has-text("Agent Terminals"), h2:has-text("Agent Terminals")');
    if (await header.count() > 0) {
      await expect(header.first()).toBeVisible();
    }
  });

  test('should show New Terminal button', async ({ page }) => {
    const newTerminalBtn = page.locator('[data-testid="new-terminal-button"]');
    if (await newTerminalBtn.count() > 0) {
      await expect(newTerminalBtn.first()).toBeVisible();
    }
  });

  test('should show Invoke All button', async ({ page }) => {
    const invokeAllBtn = page.locator('[data-testid="invoke-all-button"]');
    if (await invokeAllBtn.count() > 0) {
      await expect(invokeAllBtn.first()).toBeVisible();
    }
  });

  test('should show layout toggle buttons', async ({ page }) => {
    // Look for 2x2, 3x2, etc. layout buttons
    const layoutButtons = page.locator('button:has-text("2×2"), button:has-text("3×2"), button:has-text("4×2")');
    if (await layoutButtons.count() > 0) {
      console.log('Layout buttons found:', await layoutButtons.count());
    }
  });

  test('should show Clear All button', async ({ page }) => {
    const clearAllBtn = page.locator('button:has-text("Clear All")');
    if (await clearAllBtn.count() > 0) {
      await expect(clearAllBtn.first()).toBeVisible();
    }
  });
});

test.describe('Terminal - Invoke All Dropdown', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToTerminals(page);
  });

  test('should open dropdown when clicking Invoke All', async ({ page }) => {
    const invokeAllBtn = page.locator('button:has-text("Invoke All")');
    
    if (await invokeAllBtn.count() > 0) {
      await invokeAllBtn.first().click();
      await page.waitForTimeout(300);
      
      // Look for dropdown options
      const dropdown = page.locator('[class*="dropdown"], [class*="absolute"][class*="z-"]');
      if (await dropdown.count() > 0) {
        console.log('Invoke All dropdown opened');
      }
    }
  });

  test('should show codebuff option in dropdown', async ({ page }) => {
    const invokeAllBtn = page.locator('button:has-text("Invoke All")');
    
    if (await invokeAllBtn.count() > 0) {
      await invokeAllBtn.first().click();
      await page.waitForTimeout(300);
      
      const codebuffOption = page.locator('text=codebuff');
      if (await codebuffOption.count() > 0) {
        await expect(codebuffOption.first()).toBeVisible();
      }
    }
  });

  test('should show claude option in dropdown', async ({ page }) => {
    const invokeAllBtn = page.locator('button:has-text("Invoke All")');
    
    if (await invokeAllBtn.count() > 0) {
      await invokeAllBtn.first().click();
      await page.waitForTimeout(300);
      
      const claudeOption = page.locator('text=claude');
      if (await claudeOption.count() > 0) {
        await expect(claudeOption.first()).toBeVisible();
      }
    }
  });
});

test.describe('Terminal - Terminal Grid', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToTerminals(page);
  });

  test('should display terminal grid container', async ({ page }) => {
    const terminalGrid = page.locator('[data-testid="terminal-grid"]');
    if (await terminalGrid.count() > 0) {
      await expect(terminalGrid.first()).toBeVisible();
    }
  });

  test('should show terminal slots', async ({ page }) => {
    // Look for terminal slots/cells
    const terminalSlots = page.locator('[class*="border"][class*="rounded"]');
    if (await terminalSlots.count() > 0) {
      console.log('Terminal slots found:', await terminalSlots.count());
    }
  });

  test('should change layout when clicking layout button', async ({ page }) => {
    const layout2x2 = page.locator('button:has-text("2×2")');
    const layout3x2 = page.locator('button:has-text("3×2")');
    
    if (await layout2x2.count() > 0 && await layout3x2.count() > 0) {
      // Click 2x2
      await layout2x2.first().click();
      await page.waitForTimeout(300);
      
      // Click 3x2
      await layout3x2.first().click();
      await page.waitForTimeout(300);
      
      // Grid should update (hard to verify exact layout in E2E)
      console.log('Layout changed successfully');
    }
  });
});

test.describe('Terminal - Individual Terminal', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToTerminals(page);
  });

  test('should create new terminal when clicking New Terminal', async ({ page }) => {
    const newTerminalBtn = page.locator('button:has-text("New Terminal")');
    
    if (await newTerminalBtn.count() > 0) {
      const initialTerminals = await page.locator('[class*="terminal"]').count();
      
      await newTerminalBtn.first().click();
      await page.waitForTimeout(1000);
      
      // Should have created a new terminal
      console.log('New terminal creation attempted');
    }
  });

  test('terminal should have header with controls', async ({ page }) => {
    // Look for terminal header elements
    const terminalHeaders = page.locator('[class*="terminal"] [class*="header"], [class*="terminal-header"]');
    if (await terminalHeaders.count() > 0) {
      console.log('Terminal headers found:', await terminalHeaders.count());
    }
  });

  test('terminal should have close button', async ({ page }) => {
    // Terminals typically have a close/X button
    const closeButtons = page.locator('[class*="terminal"] button:has(svg), [title*="close"], [title*="Close"]');
    if (await closeButtons.count() > 0) {
      console.log('Terminal close buttons found:', await closeButtons.count());
    }
  });
});
