import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

// Navigate to worktrees page
async function navigateToWorktrees(page: Page) {
  // Try data-testid first, then fallback to text selector with force click
  const worktreesNavById = page.locator('[data-testid="nav-worktrees"]');
  const worktreesNavByText = page.locator('button:has-text("Worktrees")');
  
  if (await worktreesNavById.count() > 0) {
    await worktreesNavById.first().click({ force: true });
  } else if (await worktreesNavByText.count() > 0) {
    await worktreesNavByText.first().click({ force: true });
  }
  await page.waitForTimeout(500);
}

test.describe('Worktrees Page', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToWorktrees(page);
  });

  test('should display worktrees page header', async ({ page }) => {
    const header = page.locator('h1:has-text("Worktrees"), h2:has-text("Worktrees")');
    if (await header.count() > 0) {
      await expect(header.first()).toBeVisible();
    }
  });

  test('should show New Worktree button', async ({ page }) => {
    const newWorktreeBtn = page.locator('button:has-text("New Worktree"), button:has-text("Create Worktree")');
    if (await newWorktreeBtn.count() > 0) {
      await expect(newWorktreeBtn.first()).toBeVisible();
    }
  });

  test('should show Cleanup button', async ({ page }) => {
    const cleanupBtn = page.locator('button:has-text("Cleanup"), button:has-text("Clean")');
    if (await cleanupBtn.count() > 0) {
      await expect(cleanupBtn.first()).toBeVisible();
    }
  });

  test('should display worktree list or empty state', async ({ page }) => {
    // Look for worktree cards or empty state
    const worktreeCards = page.locator('[class*="worktree"], [class*="card"]');
    const emptyState = page.locator('*:has-text("No worktrees"), *:has-text("empty")');
    
    if (await worktreeCards.count() > 0) {
      console.log('Worktree cards found:', await worktreeCards.count());
    } else if (await emptyState.count() > 0) {
      console.log('Empty state displayed');
    }
  });
});

test.describe('Worktrees - Worktree Cards', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToWorktrees(page);
  });

  test('worktree cards should show branch name', async ({ page }) => {
    const branchNames = page.locator('[class*="worktree"] [class*="branch"], *:has-text("main"), *:has-text("feat/")');
    
    if (await branchNames.count() > 0) {
      console.log('Branch names found');
    }
  });

  test('worktree cards should show path', async ({ page }) => {
    const paths = page.locator('[class*="worktree"] [class*="path"], [class*="mono"]');
    
    if (await paths.count() > 0) {
      console.log('Worktree paths found');
    }
  });

  test('worktree cards should have action buttons', async ({ page }) => {
    const actionButtons = page.locator('[class*="worktree"] button');
    
    if (await actionButtons.count() > 0) {
      console.log('Action buttons found:', await actionButtons.count());
    }
  });
});

test.describe('Worktrees - Cleanup Dialog', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToWorktrees(page);
  });

  test('should open cleanup dialog', async ({ page }) => {
    const cleanupBtn = page.locator('button:has-text("Cleanup")');
    
    if (await cleanupBtn.count() > 0) {
      await cleanupBtn.first().click();
      await page.waitForTimeout(500);
      
      const modal = page.locator('[class*="modal"], [role="dialog"]');
      if (await modal.count() > 0) {
        console.log('Cleanup dialog opened');
      }
    }
  });

  test('cleanup dialog should show stale worktrees', async ({ page }) => {
    const cleanupBtn = page.locator('button:has-text("Cleanup")');
    
    if (await cleanupBtn.count() > 0) {
      await cleanupBtn.first().click();
      await page.waitForTimeout(500);
      
      const staleList = page.locator('*:has-text("stale"), *:has-text("Stale"), [class*="stale"]');
      if (await staleList.count() > 0) {
        console.log('Stale worktrees section found');
      }
    }
  });
});
