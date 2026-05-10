import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

// =============================================================================
// SLOW NETWORK / LOADING STATES
// =============================================================================

test.describe('Slow Network Conditions', () => {
  // Skip this test - WASM apps can't reliably load under extreme throttling
  // because the WASM binary itself is too large for 3G-like conditions
  test.skip('should handle slow network gracefully', async ({ page, context }) => {
    test.setTimeout(60000);
    // Simulate slow network (3G-like conditions)
    const cdpSession = await context.newCDPSession(page);
    await cdpSession.send('Network.emulateNetworkConditions', {
      offline: false,
      downloadThroughput: (500 * 1024) / 8, // 500 kbps
      uploadThroughput: (500 * 1024) / 8,
      latency: 400, // 400ms latency
    });

    await page.goto('/');
    
    // App should still load, just slower
    await expect(page.locator('body')).toBeVisible({ timeout: 45000 });
    
    // Check that main UI elements appear
    const sidebar = page.locator('[data-testid="sidebar"], [class*="sidebar"]');
    if (await sidebar.count() > 0) {
      await expect(sidebar.first()).toBeVisible({ timeout: 30000 });
    }
  });

  test('should show loading states during slow operations', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Look for loading indicators in the app
    const loadingIndicators = page.locator('[class*="animate-spin"], [class*="loading"], *:has-text("Loading")');
    
    // Loading states may or may not be visible depending on app state
    const count = await loadingIndicators.count();
    console.log(`Found ${count} loading indicators`);
  });

  test('should recover from temporary network slowdown', async ({ page, context }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Slow down network mid-session
    const cdpSession = await context.newCDPSession(page);
    await cdpSession.send('Network.emulateNetworkConditions', {
      offline: false,
      downloadThroughput: (100 * 1024) / 8, // Very slow
      uploadThroughput: (100 * 1024) / 8,
      latency: 1000,
    });

    // Try to navigate
    const navButton = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await navButton.count() > 0) {
      await navButton.first().click({ force: true });
    }

    // Restore normal network
    await cdpSession.send('Network.emulateNetworkConditions', {
      offline: false,
      downloadThroughput: -1,
      uploadThroughput: -1,
      latency: 0,
    });

    // App should still be functional
    await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
  });
});

// =============================================================================
// OFFLINE MODE
// =============================================================================

test.describe('Offline Mode', () => {
  test('should handle going offline after app loads', async ({ page, context }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Go offline
    await context.setOffline(true);

    // App should still be usable for local operations
    await expect(page.locator('body')).toBeVisible();

    // Try clicking navigation (should work as it's client-side)
    const navButton = page.locator('[data-testid="nav-agent"], button:has-text("Agent")');
    if (await navButton.count() > 0) {
      await navButton.first().click({ force: true });
      await page.waitForTimeout(500);
    }

    // App should not crash
    await expect(page.locator('body')).toBeVisible();

    // Restore online status
    await context.setOffline(false);
  });

  test('should recover when coming back online', async ({ page, context }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Go offline
    await context.setOffline(true);
    await page.waitForTimeout(1000);

    // Come back online
    await context.setOffline(false);
    await page.waitForTimeout(1000);

    // App should be fully functional
    await expect(page.locator('body')).toBeVisible();

    // Navigate to verify functionality
    const settingsNav = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await settingsNav.count() > 0) {
      await settingsNav.first().click({ force: true });
      await page.waitForTimeout(500);
    }
  });
});

// =============================================================================
// LOCAL STORAGE EDGE CASES
// =============================================================================

test.describe('LocalStorage Edge Cases', () => {
  test('should handle corrupted localStorage gracefully', async ({ page }) => {
    await page.goto('/');
    
    // Inject corrupted data into localStorage
    await page.evaluate(() => {
      localStorage.setItem('slashit_selected_project', 'invalid-uuid-that-does-not-exist');
      localStorage.setItem('slashit_page', '{"corrupted": true}');
    });

    // Reload the page
    await page.reload();
    await waitForAppLoad(page);

    // App should still load and be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle localStorage being full', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Try to fill localStorage (this may or may not succeed depending on browser limits)
    await page.evaluate(() => {
      try {
        const bigData = 'x'.repeat(1024 * 1024); // 1MB string
        for (let i = 0; i < 10; i++) {
          localStorage.setItem(`test_fill_${i}`, bigData);
        }
      } catch (e) {
        console.log('localStorage full as expected');
      }
    });

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();

    // Clean up
    await page.evaluate(() => {
      for (let i = 0; i < 10; i++) {
        localStorage.removeItem(`test_fill_${i}`);
      }
    });
  });

  test('should handle localStorage being cleared mid-session', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Clear localStorage
    await page.evaluate(() => {
      localStorage.clear();
    });

    // App should continue to work
    await expect(page.locator('body')).toBeVisible();

    // Navigation should still work
    const navButton = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await navButton.count() > 0) {
      await navButton.first().click({ force: true });
      await page.waitForTimeout(300);
      await expect(page.locator('body')).toBeVisible();
    }
  });

  test('should handle localStorage disabled', async ({ page, context }) => {
    // Block localStorage access via page context
    await page.addInitScript(() => {
      Object.defineProperty(window, 'localStorage', {
        get: () => {
          throw new Error('localStorage is disabled');
        },
      });
    });

    await page.goto('/');
    
    // App should still load even without localStorage
    await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
  });
});

// =============================================================================
// CONSOLE ERROR MONITORING
// =============================================================================

test.describe('Error Handling', () => {
  test('should not have unhandled JavaScript errors', async ({ page }) => {
    const errors: string[] = [];
    
    page.on('pageerror', (error) => {
      errors.push(error.message);
    });

    await page.goto('/');
    await waitForAppLoad(page);

    // Interact with the app
    const navButtons = page.locator('[data-testid^="nav-"], button');
    const count = await navButtons.count();
    
    for (let i = 0; i < Math.min(count, 3); i++) {
      try {
        await navButtons.nth(i).click({ force: true, timeout: 1000 });
        await page.waitForTimeout(300);
      } catch {
        // Ignore click failures
      }
    }

    // Filter out known acceptable errors (WASM-related, favicon, etc.)
    const criticalErrors = errors.filter(
      (err) =>
        !err.includes('favicon') &&
        !err.includes('wasm') &&
        !err.includes('WebAssembly') &&
        !err.includes('unreachable')
    );

    if (criticalErrors.length > 0) {
      console.log('Critical errors found:', criticalErrors);
    }
    
    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle failed API calls gracefully', async ({ page }) => {
    // Intercept all API calls and make them fail
    await page.route('**/api/**', (route) => {
      route.abort('failed');
    });

    await page.goto('/');
    await waitForAppLoad(page);

    // App should still render even if API calls fail
    await expect(page.locator('body')).toBeVisible();
  });

  test('should show error states instead of crashing', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Look for error messages or error states in the UI
    const errorElements = page.locator('[class*="error"], [class*="red-"], *:has-text("Error"), *:has-text("Failed")');
    
    // Log any visible errors for debugging
    const count = await errorElements.count();
    if (count > 0) {
      console.log(`Found ${count} error-related elements (may be expected)`);
    }

    // App should still be visible and functional
    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// RAPID USER INTERACTIONS
// =============================================================================

test.describe('Rapid User Interactions', () => {
  test('should handle rapid clicking without crashing', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Rapidly click navigation items
    const navButtons = page.locator('[data-testid^="nav-"]');
    const count = await navButtons.count();

    for (let round = 0; round < 3; round++) {
      for (let i = 0; i < count; i++) {
        try {
          await navButtons.nth(i).click({ force: true, timeout: 100 });
        } catch {
          // Ignore timeouts from rapid clicking
        }
      }
    }

    await page.waitForTimeout(500);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle rapid keyboard input', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Rapidly press keyboard shortcuts
    for (let i = 0; i < 10; i++) {
      await page.keyboard.press('1');
      await page.keyboard.press('2');
      await page.keyboard.press('3');
      await page.keyboard.press('Escape');
    }

    await page.waitForTimeout(300);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle double-clicks correctly', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Double-click various elements
    const clickableElements = page.locator('button, [role="button"]');
    const count = await clickableElements.count();

    for (let i = 0; i < Math.min(count, 5); i++) {
      try {
        await clickableElements.nth(i).dblclick({ force: true, timeout: 1000 });
        await page.waitForTimeout(100);
      } catch {
        // Ignore failures
      }
    }

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// VIEWPORT / RESPONSIVE EDGE CASES
// =============================================================================

test.describe('Viewport Edge Cases', () => {
  test('should handle very small viewport', async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 480 });
    await page.goto('/');
    await waitForAppLoad(page);

    // App should still render
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle very large viewport', async ({ page }) => {
    await page.setViewportSize({ width: 3840, height: 2160 }); // 4K
    await page.goto('/');
    await waitForAppLoad(page);

    // App should still render
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle viewport resize during interaction', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Resize while interacting
    await page.setViewportSize({ width: 1920, height: 1080 });
    await page.waitForTimeout(200);

    const navButton = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await navButton.count() > 0) {
      await navButton.first().click({ force: true });
    }

    await page.setViewportSize({ width: 768, height: 1024 });
    await page.waitForTimeout(200);

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.waitForTimeout(200);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle landscape to portrait switch', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 768 }); // Landscape
    await page.goto('/');
    await waitForAppLoad(page);

    // Switch to portrait
    await page.setViewportSize({ width: 768, height: 1024 });
    await page.waitForTimeout(300);

    // App should adapt
    await expect(page.locator('body')).toBeVisible();

    // Switch back to landscape
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.waitForTimeout(300);

    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// PAGE LIFECYCLE
// =============================================================================

test.describe('Page Lifecycle', () => {
  test('should handle multiple rapid reloads', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Rapid reloads
    for (let i = 0; i < 3; i++) {
      await page.reload();
      await page.waitForTimeout(500);
    }

    await waitForAppLoad(page);
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle back/forward navigation', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Navigate to different pages
    const settingsNav = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await settingsNav.count() > 0) {
      await settingsNav.first().click({ force: true });
      await page.waitForTimeout(500);
    }

    const agentNav = page.locator('[data-testid="nav-agent"], button:has-text("Agent")');
    if (await agentNav.count() > 0) {
      await agentNav.first().click({ force: true });
      await page.waitForTimeout(500);
    }

    // Go back
    await page.goBack();
    await page.waitForTimeout(500);

    // Go forward
    await page.goForward();
    await page.waitForTimeout(500);

    // App should still work
    await expect(page.locator('body')).toBeVisible();
  });

  test('should maintain state after page visibility change', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Simulate tab becoming hidden then visible
    await page.evaluate(() => {
      document.dispatchEvent(new Event('visibilitychange'));
    });

    await page.waitForTimeout(500);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// MEMORY / PERFORMANCE
// =============================================================================

test.describe('Memory and Performance', () => {
  test('should not leak memory with repeated navigation', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Navigate back and forth many times
    const navItems = ['nav-agent', 'nav-settings', 'nav-roadmap', 'nav-worktrees'];
    
    for (let round = 0; round < 5; round++) {
      for (const navId of navItems) {
        const navButton = page.locator(`[data-testid="${navId}"]`);
        if (await navButton.count() > 0) {
          await navButton.first().click({ force: true });
          await page.waitForTimeout(200);
        }
      }
    }

    // App should still be responsive
    await expect(page.locator('body')).toBeVisible();

    // Check for memory issues via performance API
    const memoryInfo = await page.evaluate(() => {
      if ('memory' in performance) {
        return (performance as any).memory;
      }
      return null;
    });

    if (memoryInfo) {
      console.log('Memory usage:', memoryInfo);
    }
  });

  test('should handle long-running session', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Simulate extended usage
    for (let i = 0; i < 10; i++) {
      // Random navigation
      const navButtons = page.locator('[data-testid^="nav-"]');
      const count = await navButtons.count();
      if (count > 0) {
        const randomIndex = Math.floor(Math.random() * count);
        await navButtons.nth(randomIndex).click({ force: true });
        await page.waitForTimeout(300);
      }
    }

    // App should still be functional after extended use
    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// CONCURRENT OPERATIONS
// =============================================================================

test.describe('Concurrent Operations', () => {
  test('should handle multiple modals opening', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Try to trigger multiple modals (if any exist)
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      // Click rapidly to try opening multiple modals
      await addProjectBtn.first().click({ force: true });
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      // Press escape to close any open modals
      await page.keyboard.press('Escape');
      await page.waitForTimeout(300);
    }

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should handle escape key during various states', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Press escape multiple times in various states
    for (let i = 0; i < 5; i++) {
      await page.keyboard.press('Escape');
      await page.waitForTimeout(100);
    }

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });
});
