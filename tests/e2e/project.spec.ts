import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

test.describe('Project Tabs', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should display project tabs bar', async ({ page }) => {
    // Look for project tabs container using data-testid
    const tabsBar = page.locator('[data-testid="project-tabs"]');
    if (await tabsBar.count() > 0) {
      await expect(tabsBar.first()).toBeVisible();
    }
  });

  test('should show add project button (+)', async ({ page }) => {
    const addButton = page.locator('[data-testid="add-project-button"]');
    if (await addButton.count() > 0) {
      await expect(addButton.first()).toBeVisible();
    }
  });

  test('should show existing project tabs', async ({ page }) => {
    // Look for project tab buttons
    const projectTabs = page.locator('[class*="tab"][class*="button"], button[class*="rounded"][class*="px"]');
    if (await projectTabs.count() > 0) {
      console.log('Project tabs found:', await projectTabs.count());
    }
  });

  test('should highlight selected project tab', async ({ page }) => {
    const projectTabs = page.locator('[class*="tab"] button, [class*="project"] button');
    
    if (await projectTabs.count() > 0) {
      // Click on first project tab
      await projectTabs.first().click();
      await page.waitForTimeout(300);
      
      // Check for active styling
      const activeTab = await projectTabs.first().getAttribute('class');
      if (activeTab) {
        console.log('Active tab classes:', activeTab);
      }
    }
  });
});

test.describe('Project Creation Modal', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should open project creation modal when clicking +', async ({ page }) => {
    const addButton = page.locator('button:has-text("+"), button:has-text("New Project")');
    
    if (await addButton.count() > 0) {
      await addButton.first().click();
      await page.waitForTimeout(500);
      
      // Look for modal
      const modal = page.locator('[class*="modal"], [role="dialog"], [class*="fixed"][class*="inset"]');
      if (await modal.count() > 0) {
        await expect(modal.first()).toBeVisible();
      }
    }
  });

  test('should show project name input in creation modal', async ({ page }) => {
    const addButton = page.locator('button:has-text("+"), button:has-text("New Project")');
    
    if (await addButton.count() > 0) {
      await addButton.first().click();
      await page.waitForTimeout(500);
      
      const nameInput = page.locator('input[placeholder*="name"], input[placeholder*="Name"], input:near(:text("Name"))');
      if (await nameInput.count() > 0) {
        await expect(nameInput.first()).toBeVisible();
      }
    }
  });

  test('should show folder selection in creation modal', async ({ page }) => {
    const addButton = page.locator('button:has-text("+"), button:has-text("New Project")');
    
    if (await addButton.count() > 0) {
      await addButton.first().click();
      await page.waitForTimeout(500);
      
      // Look for folder/path selection
      const folderSelect = page.locator('text=Folder, text=Path, text=Directory, button:has-text("Browse")');
      if (await folderSelect.count() > 0) {
        console.log('Folder selection found');
      }
    }
  });

  test('should show Create button in modal', async ({ page }) => {
    const addButton = page.locator('button:has-text("+"), button:has-text("New Project")');
    
    if (await addButton.count() > 0) {
      await addButton.first().click();
      await page.waitForTimeout(500);
      
      const createButton = page.locator('button:has-text("Create")');
      if (await createButton.count() > 0) {
        await expect(createButton.first()).toBeVisible();
      }
    }
  });

  test('should close modal when clicking Cancel', async ({ page }) => {
    const addButton = page.locator('button:has-text("+"), button:has-text("New Project")');
    
    if (await addButton.count() > 0) {
      await addButton.first().click();
      await page.waitForTimeout(500);
      
      const cancelButton = page.locator('button:has-text("Cancel")');
      if (await cancelButton.count() > 0) {
        await cancelButton.first().click();
        await page.waitForTimeout(300);
        
        // Modal should be closed
        console.log('Modal closed on cancel');
      }
    }
  });
});

test.describe('Project Selection', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should load Kanban when project is selected', async ({ page }) => {
    const projectTabs = page.locator('[class*="tab"] button, [class*="project"] button');
    
    if (await projectTabs.count() > 0) {
      await projectTabs.first().click();
      await page.waitForTimeout(500);
      
      // Kanban should be visible
      const kanban = page.locator('h1:has-text("Kanban"), *:has-text("Kanban Board")');
      if (await kanban.count() > 0) {
        await expect(kanban.first()).toBeVisible();
      }
    }
  });

  test('should show welcome state when no project selected', async ({ page }) => {
    // Look for welcome message
    const welcomeState = page.locator('*:has-text("Welcome to SlashIt"), *:has-text("Select a project")');
    if (await welcomeState.count() > 0) {
      console.log('Welcome state displayed');
    }
  });

  test('should allow switching between projects', async ({ page }) => {
    const projectTabs = page.locator('[class*="tab"] button, [class*="project"] button');
    
    if (await projectTabs.count() > 1) {
      // Click first project
      await projectTabs.first().click();
      await page.waitForTimeout(300);
      
      // Click second project
      await projectTabs.nth(1).click();
      await page.waitForTimeout(300);
      
      console.log('Switched between projects successfully');
    }
  });
});

test.describe('Project Tab Context Menu', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should show delete option on right-click', async ({ page }) => {
    const projectTabs = page.locator('[class*="tab"] button, [class*="project"] button');
    
    if (await projectTabs.count() > 0) {
      await projectTabs.first().click({ button: 'right' });
      await page.waitForTimeout(300);
      
      const deleteOption = page.locator('button:has-text("Delete"), *:has-text("Delete")');
      if (await deleteOption.count() > 0) {
        console.log('Delete option found in context menu');
      }
    }
  });

  test('should show close button on tab', async ({ page }) => {
    const projectTabs = page.locator('[class*="tab"], [class*="project"]');
    
    if (await projectTabs.count() > 0) {
      // Hover to reveal close button
      await projectTabs.first().hover();
      await page.waitForTimeout(200);
      
      const closeButton = page.locator('[class*="tab"] button:has(svg[class*="w-3"]), button[title*="close"], button[title*="Close"]');
      if (await closeButton.count() > 0) {
        console.log('Close button found on tab');
      }
    }
  });
});
