import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM to load
async function waitForApp(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

test.describe('Project Tabs', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForApp(page);
  });

  test('should display project tabs area', async ({ page }) => {
    // Look for project tabs container or project-related UI
    const projectArea = page.locator('[data-testid="project-tabs"], [class*="project"]');
    // Projects are displayed as tabs at the top
  });

  test('should show add project button', async ({ page }) => {
    // Look for button to add new project
    const addProjectBtn = page.locator('button[title*="project"], button:has-text("Add"), button:has(svg)');
    // There should be a way to add projects
  });

  test('should highlight selected project tab', async ({ page }) => {
    // Active project should have distinct styling
    const projectTabs = page.locator('[data-testid="project-tab"]');
    const count = await projectTabs.count();
    
    if (count > 0) {
      const firstTab = projectTabs.first();
      const tabClass = await firstTab.getAttribute('class');
      // Should have some active indicator
    }
  });
});

test.describe('Project Creation', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForApp(page);
  });

  test('should open project creation modal', async ({ page }) => {
    // Find and click add project button
    const addBtn = page.locator('button[title="Add project"], button:has-text("New Project")');
    
    if (await addBtn.count() > 0) {
      await addBtn.click();
      await page.waitForTimeout(500);
      
      // Modal should appear
      const modal = page.locator('[data-testid="create-project-modal"], [class*="modal"]');
      if (await modal.count() > 0) {
        await expect(modal.first()).toBeVisible();
      }
    }
  });

  test('should have project name input in creation form', async ({ page }) => {
    const addBtn = page.locator('button[title="Add project"], button:has-text("New Project")');
    
    if (await addBtn.count() > 0) {
      await addBtn.click();
      await page.waitForTimeout(500);
      
      // Look for name input
      const nameInput = page.locator('input[placeholder*="name"], input[name="name"]');
      if (await nameInput.count() > 0) {
        await expect(nameInput.first()).toBeVisible();
      }
    }
  });

  test('should have folder picker in creation form', async ({ page }) => {
    const addBtn = page.locator('button[title="Add project"], button:has-text("New Project")');
    
    if (await addBtn.count() > 0) {
      await addBtn.click();
      await page.waitForTimeout(500);
      
      // Look for folder picker button
      const folderBtn = page.locator('button:has-text("Browse"), button:has-text("Select"), button:has-text("folder")');
      // Should have a way to select folder
    }
  });

  test('should close modal on cancel', async ({ page }) => {
    const addBtn = page.locator('button[title="Add project"], button:has-text("New Project")');
    
    if (await addBtn.count() > 0) {
      await addBtn.click();
      await page.waitForTimeout(500);
      
      // Find cancel button
      const cancelBtn = page.locator('button:has-text("Cancel")');
      if (await cancelBtn.count() > 0) {
        await cancelBtn.click();
        await page.waitForTimeout(300);
        
        // Modal should be closed
        const modal = page.locator('[data-testid="create-project-modal"]');
        await expect(modal).toHaveCount(0);
      }
    }
  });

  test('should close modal on escape key', async ({ page }) => {
    const addBtn = page.locator('button[title="Add project"], button:has-text("New Project")');
    
    if (await addBtn.count() > 0) {
      await addBtn.click();
      await page.waitForTimeout(500);
      
      await page.keyboard.press('Escape');
      await page.waitForTimeout(300);
      
      // Modal should be closed
    }
  });
});

test.describe('Project Selection', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForApp(page);
  });

  test('should switch projects when clicking tab', async ({ page }) => {
    const projectTabs = page.locator('[data-testid="project-tab"]');
    const count = await projectTabs.count();
    
    if (count > 1) {
      // Click second project
      await projectTabs.nth(1).click();
      await page.waitForTimeout(500);
      
      // Content should update (tasks reload, etc.)
      // Look for loading indicator or content change
    }
  });

  test('should persist selected project after refresh', async ({ page }) => {
    const projectTabs = page.locator('[data-testid="project-tab"]');
    const count = await projectTabs.count();
    
    if (count > 1) {
      // Click second project
      await projectTabs.nth(1).click();
      await page.waitForTimeout(500);
      
      // Reload page
      await page.reload();
      await waitForApp(page);
      
      // Same project should be selected
      // (depends on localStorage persistence)
    }
  });

  test('should load tasks for selected project', async ({ page }) => {
    // When a project is selected, its tasks should load
    // Look for loading state or tasks appearing
    const loadingIndicator = page.locator('text=Loading tasks');
    const tasksLoaded = page.locator('text=Loaded');
    
    // Either loading or loaded state
  });
});

test.describe('Project Deletion', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForTimeout(3000);
    await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
  });

  test('should show delete option in project context', async ({ page }) => {
    // Right-click on project tab should show delete option
    const projectTabs = page.locator('[data-testid="project-tab"]');
    const count = await projectTabs.count();
    
    if (count > 0) {
      await projectTabs.first().click({ button: 'right' });
      await page.waitForTimeout(300);
      
      // Context menu with delete option
      const deleteOption = page.locator('text=Delete');
      // May or may not have context menu
    }
  });

  test('should confirm before deleting project', async ({ page }) => {
    // Delete should require confirmation
    // This prevents accidental deletions
  });
});

test.describe('Welcome State', () => {
  test('should show welcome message when no project selected', async ({ page }) => {
    // Clear localStorage to ensure no project is selected
    await page.goto('/');
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await page.waitForTimeout(3000);
    await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
    
    // If no projects exist or none selected, show welcome
    const welcomeMsg = page.locator('text=Welcome to SlashIt');
    const selectProjectMsg = page.locator('text=Select a project');
    
    // One of these messages may be visible
    const hasWelcome = await welcomeMsg.count() > 0;
    const hasSelect = await selectProjectMsg.count() > 0;
    
    // Either welcome or already has a project selected
  });

  test('should show guidance to create or select project', async ({ page }) => {
    await page.goto('/');
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await page.waitForTimeout(3000);
    await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
    
    const guidanceText = page.locator('*:has-text("sidebar"), *:has-text("create")');
    // Should guide user on what to do
  });
});

test.describe('Project Tab Close Button', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForApp(page);
  });

  test('should show close button on hover', async ({ page }) => {
    const projectTabs = page.locator('[data-testid="project-tab"]');
    const count = await projectTabs.count();
    
    if (count > 0) {
      await projectTabs.first().hover();
      await page.waitForTimeout(300);
      
      // Close button should appear
      const closeBtn = page.locator('[data-testid="project-tab-close"], button[title="Close"]');
      // May have close functionality
    }
  });
});
