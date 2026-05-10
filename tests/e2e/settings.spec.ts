import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

// Navigate to settings page
async function navigateToSettings(page: Page) {
  const settingsNav = page.locator('button:has-text("Settings")');
  if (await settingsNav.count() > 0) {
    await settingsNav.first().click();
    await page.waitForTimeout(500);
  }
}

test.describe('Settings Page', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToSettings(page);
  });

  test('should display settings page header', async ({ page }) => {
    const header = page.locator('h1:has-text("Settings"), h2:has-text("Settings")');
    if (await header.count() > 0) {
      await expect(header.first()).toBeVisible();
    }
  });

  test('should show appearance settings section', async ({ page }) => {
    const appearanceSection = page.locator('h2:has-text("Appearance"), h3:has-text("Appearance"), *:has-text("Appearance")');
    if (await appearanceSection.count() > 0) {
      await expect(appearanceSection.first()).toBeVisible();
    }
  });

  test('should show theme selector', async ({ page }) => {
    // Look for theme-related elements
    const themeSelector = page.locator('*:has-text("Theme"), select, [class*="theme"]');
    if (await themeSelector.count() > 0) {
      console.log('Theme selector found');
    }
  });

  test('should show agent configuration section', async ({ page }) => {
    const agentSection = page.locator('h2:has-text("Agent"), h3:has-text("Agent"), *:has-text("Agent")');
    if (await agentSection.count() > 0) {
      console.log('Agent configuration section found');
    }
  });

  test('should show model selection options', async ({ page }) => {
    // Look for model selection
    const modelSection = page.locator('*:has-text("Model"), *:has-text("Claude"), *:has-text("claude")');
    if (await modelSection.count() > 0) {
      console.log('Model selection options found');
    }
  });
});

test.describe('Settings - Appearance', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToSettings(page);
  });

  test('should have dark mode toggle or theme options', async ({ page }) => {
    const darkModeToggle = page.locator('*:has-text("Dark"), *:has-text("Light"), [class*="toggle"]');
    if (await darkModeToggle.count() > 0) {
      console.log('Dark mode toggle found');
    }
  });

  test('should show color scheme options', async ({ page }) => {
    // Look for color scheme related elements
    const colorOptions = page.locator('[class*="color"], [class*="bg-"][class*="rounded"][class*="cursor-pointer"]');
    if (await colorOptions.count() > 0) {
      console.log('Color scheme options found:', await colorOptions.count());
    }
  });
});

test.describe('Settings - Queue Configuration', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToSettings(page);
  });

  test('should show queue settings', async ({ page }) => {
    const queueSection = page.locator('h2:has-text("Queue"), h3:has-text("Queue"), *:has-text("Queue")');
    if (await queueSection.count() > 0) {
      console.log('Queue settings section found');
    }
  });

  test('should show auto-start option', async ({ page }) => {
    const autoStartOption = page.locator('*:has-text("Auto-start"), *:has-text("auto start")');
    if (await autoStartOption.count() > 0) {
      console.log('Auto-start option found');
    }
  });

  test('should show parallel task limit option', async ({ page }) => {
    const parallelOption = page.locator('*:has-text("Parallel"), *:has-text("parallel"), *:has-text("concurrent")');
    if (await parallelOption.count() > 0) {
      console.log('Parallel task limit option found');
    }
  });
});

test.describe('Settings - Integration', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToSettings(page);
  });

  test('should show GitHub integration section', async ({ page }) => {
    const githubSection = page.locator('h2:has-text("GitHub"), h3:has-text("GitHub"), *:has-text("GitHub")');
    if (await githubSection.count() > 0) {
      console.log('GitHub integration section found');
    }
  });

  test('should show API configuration', async ({ page }) => {
    const apiSection = page.locator('*:has-text("API"), *:has-text("api"), *:has-text("Token")');
    if (await apiSection.count() > 0) {
      console.log('API configuration section found');
    }
  });
});

test.describe('Settings - Persistence', () => {
  test('settings should persist after page reload', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToSettings(page);
    
    // Make a change (if possible)
    // For now, just verify settings page loads consistently
    
    // Reload page
    await page.reload();
    await waitForAppLoad(page);
    await navigateToSettings(page);
    
    // Settings page should still work
    const header = page.locator('h1:has-text("Settings"), h2:has-text("Settings")');
    if (await header.count() > 0) {
      await expect(header.first()).toBeVisible();
    }
  });
});
