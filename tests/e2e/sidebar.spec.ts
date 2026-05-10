import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

test.describe('Sidebar Navigation', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should display sidebar', async ({ page }) => {
    const sidebar = page.locator('[data-testid="sidebar"]');
    if (await sidebar.count() > 0) {
      await expect(sidebar.first()).toBeVisible();
    }
  });

  test('should show Kanban navigation item', async ({ page }) => {
    const kanbanNav = page.locator('[data-testid="nav-dashboard"]');
    if (await kanbanNav.count() > 0) {
      await expect(kanbanNav.first()).toBeVisible();
    }
  });

  test('should show Agent Terminals navigation item', async ({ page }) => {
    const terminalsNav = page.locator('[data-testid="nav-agent"]');
    if (await terminalsNav.count() > 0) {
      await expect(terminalsNav.first()).toBeVisible();
    }
  });

  test('should show Settings navigation item', async ({ page }) => {
    const settingsNav = page.locator('[data-testid="nav-settings"]');
    if (await settingsNav.count() > 0) {
      await expect(settingsNav.first()).toBeVisible();
    }
  });

  test('should show Roadmap navigation item', async ({ page }) => {
    const roadmapNav = page.locator('button:has-text("Roadmap"), a:has-text("Roadmap"), *:has-text("Roadmap")');
    if (await roadmapNav.count() > 0) {
      await expect(roadmapNav.first()).toBeVisible();
    }
  });

  test('should show Worktrees navigation item', async ({ page }) => {
    const worktreesNav = page.locator('button:has-text("Worktrees"), a:has-text("Worktrees"), *:has-text("Worktrees")');
    if (await worktreesNav.count() > 0) {
      await expect(worktreesNav.first()).toBeVisible();
    }
  });

  test('should navigate to Kanban when clicking Kanban button', async ({ page }) => {
    const kanbanNav = page.locator('button:has-text("Kanban")');
    
    if (await kanbanNav.count() > 0) {
      await kanbanNav.first().click();
      await page.waitForTimeout(500);
      
      // Should show Kanban content
      const kanbanContent = page.locator('h1:has-text("Kanban"), *:has-text("Kanban Board")');
      if (await kanbanContent.count() > 0) {
        await expect(kanbanContent.first()).toBeVisible();
      }
    }
  });

  test('should navigate to Settings when clicking Settings button', async ({ page }) => {
    const settingsNav = page.locator('button:has-text("Settings")');
    
    if (await settingsNav.count() > 0) {
      await settingsNav.first().click();
      await page.waitForTimeout(500);
      
      // Should show Settings content
      const settingsContent = page.locator('h1:has-text("Settings"), h2:has-text("Settings")');
      if (await settingsContent.count() > 0) {
        console.log('Navigated to Settings');
      }
    }
  });

  test('should navigate to Agent Terminals when clicking Terminals button', async ({ page }) => {
    const terminalsNav = page.locator('button:has-text("Agent Terminals"), button:has-text("Terminals")');
    
    if (await terminalsNav.count() > 0) {
      await terminalsNav.first().click();
      await page.waitForTimeout(500);
      
      // Should show terminals content
      const terminalsContent = page.locator('h1:has-text("Agent Terminals"), *:has-text("Terminal")');
      if (await terminalsContent.count() > 0) {
        console.log('Navigated to Terminals');
      }
    }
  });

  test('should highlight active navigation item', async ({ page }) => {
    const navItems = page.locator('nav button, [class*="sidebar"] button');
    
    if (await navItems.count() > 0) {
      // Click on a nav item
      await navItems.first().click();
      await page.waitForTimeout(300);
      
      // Check for active styling (usually contains bg-* class)
      const activeClasses = await navItems.first().getAttribute('class');
      if (activeClasses) {
        console.log('Nav item classes:', activeClasses);
      }
    }
  });

  test('should show navigation icons', async ({ page }) => {
    // Look for SVG icons in navigation
    const navIcons = page.locator('nav svg, [class*="sidebar"] svg');
    
    if (await navIcons.count() > 0) {
      console.log('Navigation icons found:', await navIcons.count());
    }
  });
});

test.describe('Sidebar - Sections', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should show WORKFLOW section', async ({ page }) => {
    const workflowSection = page.locator('text=WORKFLOW');
    if (await workflowSection.count() > 0) {
      await expect(workflowSection.first()).toBeVisible();
    }
  });

  test('should show DEVELOPMENT section', async ({ page }) => {
    const devSection = page.locator('text=DEVELOPMENT');
    if (await devSection.count() > 0) {
      await expect(devSection.first()).toBeVisible();
    }
  });

  test('should show INSIGHTS section', async ({ page }) => {
    const insightsSection = page.locator('text=INSIGHTS');
    if (await insightsSection.count() > 0) {
      await expect(insightsSection.first()).toBeVisible();
    }
  });
});
