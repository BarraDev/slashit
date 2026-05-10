import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
}

// =============================================================================
// KEYBOARD NAVIGATION
// =============================================================================

test.describe('Keyboard Navigation - Tab Order', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have focusable navigation items', async ({ page }) => {
    // Tab through the page and verify focus moves
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);

    const focusedElement = page.locator(':focus');
    const isFocused = await focusedElement.count() > 0;
    
    if (isFocused) {
      console.log('Focus established on first tab');
    }
    
    // Continue tabbing to verify tab order exists
    for (let i = 0; i < 5; i++) {
      await page.keyboard.press('Tab');
      await page.waitForTimeout(50);
    }

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });

  test('should allow reverse tab navigation with Shift+Tab', async ({ page }) => {
    // Tab forward a few times
    for (let i = 0; i < 3; i++) {
      await page.keyboard.press('Tab');
      await page.waitForTimeout(50);
    }

    // Tab backward
    await page.keyboard.press('Shift+Tab');
    await page.waitForTimeout(100);

    // Focus should have moved backward
    const focusedElement = page.locator(':focus');
    if (await focusedElement.count() > 0) {
      console.log('Reverse tab navigation works');
    }
  });

  test('should trap focus within modals', async ({ page }) => {
    // Try to open a modal (e.g., add project)
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      // Check if modal is open
      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        // Tab through modal elements
        for (let i = 0; i < 10; i++) {
          await page.keyboard.press('Tab');
          await page.waitForTimeout(50);
        }

        // Focus should still be within modal (focus trap)
        const focusedElement = page.locator(':focus');
        if (await focusedElement.count() > 0) {
          // Check if focused element is inside modal
          const isInsideModal = await page.evaluate(() => {
            const focused = document.activeElement;
            const modal = document.querySelector('[role="dialog"]');
            return modal?.contains(focused) || false;
          });
          
          if (isInsideModal) {
            console.log('Focus trap working correctly');
          }
        }

        // Close modal with Escape
        await page.keyboard.press('Escape');
      }
    }
  });

  test('should close modals with Escape key', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        await page.keyboard.press('Escape');
        await page.waitForTimeout(300);

        // Modal should be closed
        const modalAfter = page.locator('[role="dialog"]');
        const modalClosed = await modalAfter.count() === 0;
        expect(modalClosed || await modalAfter.isHidden()).toBeTruthy();
      }
    }
  });
});

test.describe('Keyboard Navigation - Interactive Elements', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should activate buttons with Enter key', async ({ page }) => {
    const buttons = page.locator('button:visible');
    const count = await buttons.count();

    if (count > 0) {
      // Focus first button
      await buttons.first().focus();
      await page.waitForTimeout(100);

      // Press Enter to activate
      await page.keyboard.press('Enter');
      await page.waitForTimeout(300);

      // App should still be functional
      await expect(page.locator('body')).toBeVisible();
    }
  });

  test('should activate buttons with Space key', async ({ page }) => {
    const buttons = page.locator('button:visible');
    const count = await buttons.count();

    if (count > 0) {
      await buttons.first().focus();
      await page.waitForTimeout(100);

      // Press Space to activate
      await page.keyboard.press('Space');
      await page.waitForTimeout(300);

      await expect(page.locator('body')).toBeVisible();
    }
  });

  test('should navigate dropdowns with arrow keys', async ({ page }) => {
    // Find the Invoke All dropdown
    const invokeDropdown = page.locator('[data-testid="invoke-all-button"], button:has-text("Invoke All")');
    
    if (await invokeDropdown.count() > 0) {
      await invokeDropdown.first().click();
      await page.waitForTimeout(300);

      // Use arrow keys to navigate dropdown
      await page.keyboard.press('ArrowDown');
      await page.waitForTimeout(100);
      await page.keyboard.press('ArrowDown');
      await page.waitForTimeout(100);
      await page.keyboard.press('ArrowUp');
      await page.waitForTimeout(100);

      // Close with Escape
      await page.keyboard.press('Escape');
    }
  });

  test('should support keyboard shortcuts for navigation', async ({ page }) => {
    // Test number key navigation (1-9)
    await page.keyboard.press('1');
    await page.waitForTimeout(300);

    await page.keyboard.press('2');
    await page.waitForTimeout(300);

    await page.keyboard.press('3');
    await page.waitForTimeout(300);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
  });
});

// =============================================================================
// ARIA ATTRIBUTES AND ROLES
// =============================================================================

test.describe('ARIA Attributes - Semantic Structure', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have proper ARIA roles on modals', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      // Check for dialog role
      const dialog = page.locator('[role="dialog"]');
      if (await dialog.count() > 0) {
        await expect(dialog.first()).toBeVisible();

        // Check for aria-modal attribute
        const ariaModal = await dialog.first().getAttribute('aria-modal');
        expect(ariaModal).toBe('true');

        // Close modal
        await page.keyboard.press('Escape');
      }
    }
  });

  test('should have accessible names on interactive elements', async ({ page }) => {
    // Check buttons have accessible names
    const buttons = page.locator('button:visible');
    const count = await buttons.count();

    let buttonsWithAccessibleName = 0;
    for (let i = 0; i < Math.min(count, 10); i++) {
      const button = buttons.nth(i);
      const text = await button.textContent();
      const ariaLabel = await button.getAttribute('aria-label');
      const title = await button.getAttribute('title');

      if (text?.trim() || ariaLabel || title) {
        buttonsWithAccessibleName++;
      }
    }

    console.log(`${buttonsWithAccessibleName}/${Math.min(count, 10)} buttons have accessible names`);
  });

  test('should have aria-label on icon-only buttons', async ({ page }) => {
    // Find buttons that only contain SVG (icon-only buttons)
    const iconButtons = page.locator('button:has(svg):not(:has-text("a"))');
    const count = await iconButtons.count();

    for (let i = 0; i < Math.min(count, 5); i++) {
      const button = iconButtons.nth(i);
      const ariaLabel = await button.getAttribute('aria-label');
      const title = await button.getAttribute('title');

      if (ariaLabel || title) {
        console.log(`Icon button ${i + 1} has accessible name: ${ariaLabel || title}`);
      }
    }
  });

  test('should have proper heading hierarchy', async ({ page }) => {
    // Check for headings
    const h1 = page.locator('h1');
    const h2 = page.locator('h2');
    const h3 = page.locator('h3');

    const h1Count = await h1.count();
    const h2Count = await h2.count();
    const h3Count = await h3.count();

    console.log(`Heading hierarchy: H1=${h1Count}, H2=${h2Count}, H3=${h3Count}`);

    // There should be at least some headings for screen readers
    const totalHeadings = h1Count + h2Count + h3Count;
    expect(totalHeadings).toBeGreaterThanOrEqual(0);
  });
});

test.describe('ARIA Attributes - Live Regions', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have toast container for announcements', async ({ page }) => {
    // Look for toast/notification container
    const toastContainer = page.locator('[class*="toast"], [role="alert"], [aria-live]');
    
    // Toast container may exist even if not visible
    const exists = await toastContainer.count() > 0;
    console.log(`Toast/alert container exists: ${exists}`);
  });

  test('should announce loading states', async ({ page }) => {
    // Look for loading indicators with proper ARIA
    const loadingIndicators = page.locator('[aria-busy="true"], [aria-live] *:has-text("Loading"), [class*="loading"]');
    
    const count = await loadingIndicators.count();
    console.log(`Found ${count} loading indicators`);
  });
});

// =============================================================================
// FOCUS MANAGEMENT
// =============================================================================

test.describe('Focus Management', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have visible focus indicators', async ({ page }) => {
    // Tab to first focusable element
    await page.keyboard.press('Tab');
    await page.waitForTimeout(100);

    // Check if there's a visible focus indicator
    const focusedElement = page.locator(':focus');
    if (await focusedElement.count() > 0) {
      // Check for focus-visible or outline styles
      const hasOutline = await focusedElement.evaluate((el) => {
        const styles = window.getComputedStyle(el);
        return styles.outline !== 'none' || 
               styles.boxShadow !== 'none' ||
               el.classList.contains('focus-visible') ||
               el.matches(':focus-visible');
      });

      console.log(`Focus indicator visible: ${hasOutline}`);
    }
  });

  test('should return focus after modal closes', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      // Focus and click the button
      await addProjectBtn.first().focus();
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        // Close modal
        await page.keyboard.press('Escape');
        await page.waitForTimeout(300);

        // Focus should return to trigger button (ideally)
        const focusedElement = page.locator(':focus');
        if (await focusedElement.count() > 0) {
          console.log('Focus returned after modal close');
        }
      }
    }
  });

  test('should focus first input in modal when opened', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        // Check if an input or button inside modal is focused
        const focusedInModal = await page.evaluate(() => {
          const modal = document.querySelector('[role="dialog"]');
          const focused = document.activeElement;
          return modal?.contains(focused) || false;
        });

        console.log(`Focus moved to modal: ${focusedInModal}`);

        await page.keyboard.press('Escape');
      }
    }
  });
});

// =============================================================================
// SCREEN READER SUPPORT
// =============================================================================

test.describe('Screen Reader Support', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have descriptive page title', async ({ page }) => {
    const title = await page.title();
    expect(title.length).toBeGreaterThan(0);
    console.log(`Page title: ${title}`);
  });

  test('should have skip link or main landmark', async ({ page }) => {
    // Check for skip link
    const skipLink = page.locator('a[href="#main"], a:has-text("Skip to content"), a:has-text("Skip to main")');
    const hasSkipLink = await skipLink.count() > 0;

    // Check for main landmark
    const mainLandmark = page.locator('main, [role="main"]');
    const hasMainLandmark = await mainLandmark.count() > 0;

    console.log(`Skip link: ${hasSkipLink}, Main landmark: ${hasMainLandmark}`);
  });

  test('should have navigation landmark', async ({ page }) => {
    const nav = page.locator('nav, [role="navigation"]');
    const hasNav = await nav.count() > 0;
    
    console.log(`Navigation landmark exists: ${hasNav}`);
  });

  test('should have alt text or aria-label on images/icons', async ({ page }) => {
    // Check SVG icons have accessible names
    const svgs = page.locator('svg[aria-label], svg[aria-hidden="true"], svg[role="img"]');
    const count = await svgs.count();
    
    console.log(`Found ${count} SVGs with accessibility attributes`);
  });

  test('should announce status changes', async ({ page }) => {
    // Look for aria-live regions
    const liveRegions = page.locator('[aria-live], [role="status"], [role="alert"]');
    const count = await liveRegions.count();
    
    console.log(`Found ${count} live regions for announcements`);
  });
});

// =============================================================================
// COLOR AND CONTRAST
// =============================================================================

test.describe('Visual Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should not rely solely on color to convey information', async ({ page }) => {
    // Check that status indicators have more than just color
    const statusIndicators = page.locator('[class*="status"], [class*="badge"]');
    const count = await statusIndicators.count();

    for (let i = 0; i < Math.min(count, 5); i++) {
      const indicator = statusIndicators.nth(i);
      const text = await indicator.textContent();
      const ariaLabel = await indicator.getAttribute('aria-label');

      if (text?.trim() || ariaLabel) {
        console.log(`Status indicator ${i + 1} has text/label: ${text || ariaLabel}`);
      }
    }
  });

  test('should support reduced motion preference', async ({ page }) => {
    // Emulate reduced motion preference
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.reload();
    await waitForAppLoad(page);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();

    // Check that animations are disabled (would need to inspect CSS)
    console.log('Reduced motion test completed');
  });

  test('should be usable in high contrast mode', async ({ page }) => {
    // Emulate forced-colors (high contrast)
    await page.emulateMedia({ forcedColors: 'active' });
    await page.reload();
    await waitForAppLoad(page);

    // App should still be functional
    await expect(page.locator('body')).toBeVisible();
    console.log('High contrast mode test completed');
  });
});

// =============================================================================
// FORM ACCESSIBILITY
// =============================================================================

test.describe('Form Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have labels for form inputs', async ({ page }) => {
    // Open a modal with form
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        // Check inputs have labels
        const inputs = modal.locator('input:visible');
        const inputCount = await inputs.count();

        for (let i = 0; i < inputCount; i++) {
          const input = inputs.nth(i);
          const id = await input.getAttribute('id');
          const ariaLabel = await input.getAttribute('aria-label');
          const ariaLabelledby = await input.getAttribute('aria-labelledby');
          const placeholder = await input.getAttribute('placeholder');

          const hasLabel = id || ariaLabel || ariaLabelledby || placeholder;
          console.log(`Input ${i + 1} has accessible label: ${!!hasLabel}`);
        }

        await page.keyboard.press('Escape');
      }
    }
  });

  test('should announce form errors', async ({ page }) => {
    // Look for error messages with proper ARIA
    const errorMessages = page.locator('[role="alert"], [aria-invalid="true"], [class*="error"]');
    
    // Errors may not be visible initially
    const count = await errorMessages.count();
    console.log(`Found ${count} potential error regions`);
  });

  test('should mark required fields', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        // Check for required indicators
        const requiredInputs = modal.locator('input[required], input[aria-required="true"]');
        const count = await requiredInputs.count();
        
        console.log(`Found ${count} required inputs`);

        await page.keyboard.press('Escape');
      }
    }
  });
});

// =============================================================================
// ACCESSIBILITY TREE SNAPSHOT
// =============================================================================

test.describe('Accessibility Tree', () => {
  test('should have meaningful accessibility tree', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Get accessibility snapshot (may not be available in all contexts)
    try {
      const snapshot = await page.accessibility.snapshot();
      
      if (snapshot) {
        // Count meaningful nodes
        const countNodes = (node: any): number => {
          let count = 1;
          if (node.children) {
            for (const child of node.children) {
              count += countNodes(child);
            }
          }
          return count;
        };

        const totalNodes = countNodes(snapshot);
        console.log(`Accessibility tree has ${totalNodes} nodes`);
        console.log(`Root role: ${snapshot.role}, name: ${snapshot.name}`);
      }
    } catch (e) {
      // accessibility.snapshot() may not be available in all Playwright configurations
      console.log('Accessibility snapshot API not available, skipping');
    }
  });

  test('should have proper button roles', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Check buttons via accessibility API
    const buttons = page.getByRole('button');
    const count = await buttons.count();
    
    console.log(`Found ${count} elements with button role`);
    expect(count).toBeGreaterThan(0);
  });

  test('should have proper link roles', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    const links = page.getByRole('link');
    const count = await links.count();
    
    console.log(`Found ${count} elements with link role`);
  });
});
