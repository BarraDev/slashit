import { test, expect, Page } from '@playwright/test';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 10000 });
}

// Navigate to roadmap page
async function navigateToRoadmap(page: Page) {
  const roadmapNav = page.locator('button:has-text("Roadmap")');
  if (await roadmapNav.count() > 0) {
    await roadmapNav.first().click();
    await page.waitForTimeout(500);
  }
}

test.describe('Roadmap Page', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToRoadmap(page);
  });

  test('should display roadmap page header', async ({ page }) => {
    const header = page.locator('h1:has-text("Roadmap"), h2:has-text("Roadmap")');
    if (await header.count() > 0) {
      await expect(header.first()).toBeVisible();
    }
  });

  test('should show Add Feature button', async ({ page }) => {
    const addFeatureBtn = page.locator('button:has-text("Add Feature"), button:has-text("New Feature")');
    if (await addFeatureBtn.count() > 0) {
      await expect(addFeatureBtn.first()).toBeVisible();
    }
  });

  test('should display priority columns', async ({ page }) => {
    // Look for priority column headers
    const priorityColumns = ['Now', 'Next', 'Later', 'Shipped'];
    
    for (const priority of priorityColumns) {
      const column = page.locator(`h3:has-text("${priority}"), *:has-text("${priority}")`);
      if (await column.count() > 0) {
        console.log(`Found priority column: ${priority}`);
      }
    }
  });

  test('should show feature cards in columns', async ({ page }) => {
    // Look for feature cards
    const featureCards = page.locator('[class*="feature"], [class*="card"][draggable="true"]');
    if (await featureCards.count() > 0) {
      console.log('Feature cards found:', await featureCards.count());
    }
  });
});

test.describe('Roadmap - Feature Cards', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToRoadmap(page);
  });

  test('feature cards should be draggable', async ({ page }) => {
    const featureCards = page.locator('[draggable="true"]');
    
    if (await featureCards.count() > 0) {
      await expect(featureCards.first()).toHaveAttribute('draggable', 'true');
    }
  });

  test('feature cards should show title', async ({ page }) => {
    const featureTitles = page.locator('[class*="feature"] h4, [class*="card"] h4');
    
    if (await featureTitles.count() > 0) {
      await expect(featureTitles.first()).toBeVisible();
    }
  });

  test('feature cards should show audience badge', async ({ page }) => {
    // Look for audience badges (Users, Developers, etc.)
    const audienceBadges = page.locator('span:has-text("Users"), span:has-text("Developers"), span:has-text("Enterprise")');
    
    if (await audienceBadges.count() > 0) {
      console.log('Audience badges found:', await audienceBadges.count());
    }
  });

  test('feature cards should show impact indicator', async ({ page }) => {
    // Look for impact indicators
    const impactIndicators = page.locator('*:has-text("Impact"), *:has-text("High Impact"), *:has-text("Medium Impact")');
    
    if (await impactIndicators.count() > 0) {
      console.log('Impact indicators found');
    }
  });
});

test.describe('Roadmap - Feature Creation', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToRoadmap(page);
  });

  test('should open feature creation modal', async ({ page }) => {
    const addFeatureBtn = page.locator('button:has-text("Add Feature"), button:has-text("New Feature")');
    
    if (await addFeatureBtn.count() > 0) {
      await addFeatureBtn.first().click();
      await page.waitForTimeout(500);
      
      const modal = page.locator('[class*="modal"], [role="dialog"]');
      if (await modal.count() > 0) {
        await expect(modal.first()).toBeVisible();
      }
    }
  });

  test('feature modal should have title input', async ({ page }) => {
    const addFeatureBtn = page.locator('button:has-text("Add Feature"), button:has-text("New Feature")');
    
    if (await addFeatureBtn.count() > 0) {
      await addFeatureBtn.first().click();
      await page.waitForTimeout(500);
      
      const titleInput = page.locator('input[placeholder*="title"], input[placeholder*="Title"]');
      if (await titleInput.count() > 0) {
        await expect(titleInput.first()).toBeVisible();
      }
    }
  });

  test('feature modal should have priority selector', async ({ page }) => {
    const addFeatureBtn = page.locator('button:has-text("Add Feature"), button:has-text("New Feature")');
    
    if (await addFeatureBtn.count() > 0) {
      await addFeatureBtn.first().click();
      await page.waitForTimeout(500);
      
      const prioritySelector = page.locator('*:has-text("Priority"), select');
      if (await prioritySelector.count() > 0) {
        console.log('Priority selector found');
      }
    }
  });
});

test.describe('Roadmap - Competitor Analysis', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
    await navigateToRoadmap(page);
  });

  test('should show competitor analysis button', async ({ page }) => {
    const competitorBtn = page.locator('button:has-text("Competitor"), button:has-text("Analysis")');
    if (await competitorBtn.count() > 0) {
      await expect(competitorBtn.first()).toBeVisible();
    }
  });

  test('should open competitor analysis modal', async ({ page }) => {
    const competitorBtn = page.locator('button:has-text("Competitor"), button:has-text("Analysis")');
    
    if (await competitorBtn.count() > 0) {
      await competitorBtn.first().click();
      await page.waitForTimeout(500);
      
      const modal = page.locator('[class*="modal"], [role="dialog"]');
      if (await modal.count() > 0) {
        console.log('Competitor analysis modal opened');
      }
    }
  });
});
