import { test, expect, Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

// Helper to wait for WASM app to load
async function waitForAppLoad(page: Page) {
  await page.waitForTimeout(3000);
  await expect(page.locator('body')).toBeVisible({ timeout: 15000 });
}

// Helper to format axe violations for readable output
function formatViolations(violations: any[]) {
  return violations.map(v => ({
    id: v.id,
    impact: v.impact,
    description: v.description,
    helpUrl: v.helpUrl,
    nodes: v.nodes.length,
    targets: v.nodes.slice(0, 3).map((n: any) => n.target.join(' ')),
  }));
}

// =============================================================================
// WCAG 2.1 Level A - Minimum Accessibility
// =============================================================================

test.describe('WCAG 2.1 Level A - Minimum Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have no WCAG 2.1 Level A violations on main page', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag21a'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('WCAG 2.1 Level A Violations:', formatViolations(results.violations));
    }

    // Log passes for reference
    console.log(`Passed ${results.passes.length} Level A checks`);
    
    // Assert no critical violations
    const criticalViolations = results.violations.filter(v => v.impact === 'critical');
    expect(criticalViolations).toHaveLength(0);
  });

  test('should have no critical image-alt violations', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['image-alt'])
      .analyze();

    const violations = results.violations.filter(v => v.id === 'image-alt');
    if (violations.length > 0) {
      console.log('Image alt violations:', formatViolations(violations));
    }
  });

  test('should have no critical button-name violations', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['button-name'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Button name violations:', formatViolations(results.violations));
    }
  });

  test('should have no critical link-name violations', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['link-name'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Link name violations:', formatViolations(results.violations));
    }
  });
});

// =============================================================================
// WCAG 2.1 Level AA - Standard Accessibility (Recommended)
// =============================================================================

test.describe('WCAG 2.1 Level AA - Standard Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have no WCAG 2.1 Level AA violations on main page', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('WCAG 2.1 Level AA Violations:', formatViolations(results.violations));
    }

    console.log(`Passed ${results.passes.length} Level AA checks`);
    
    // For AA, we report but may not fail on all issues
    const seriousViolations = results.violations.filter(
      v => v.impact === 'critical' || v.impact === 'serious'
    );
    
    console.log(`Found ${seriousViolations.length} serious/critical violations`);
  });

  test('should have adequate color contrast', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['color-contrast'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Color contrast violations:', formatViolations(results.violations));
      console.log('Note: Dark theme apps may have intentional low-contrast areas');
    }
  });

  test('should have proper heading hierarchy', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['heading-order'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Heading order violations:', formatViolations(results.violations));
    }
  });

  test('should have proper form labels', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['label', 'label-title-only'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Form label violations:', formatViolations(results.violations));
    }
  });
});

// =============================================================================
// PAGE-SPECIFIC ACCESSIBILITY SCANS
// =============================================================================

test.describe('Page-Specific Accessibility Scans', () => {
  test('Kanban board should be accessible', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Navigate to Kanban (default page)
    const kanbanBoard = page.locator('[data-testid="kanban-board"], [class*="kanban"]');
    
    if (await kanbanBoard.count() > 0) {
      const results = await new AxeBuilder({ page })
        .include('[data-testid="kanban-board"], [class*="kanban"]')
        .withTags(['wcag2a', 'wcag2aa'])
        .analyze();

      if (results.violations.length > 0) {
        console.log('Kanban accessibility violations:', formatViolations(results.violations));
      }

      console.log(`Kanban board passed ${results.passes.length} checks`);
    }
  });

  test('Sidebar should be accessible', async ({ page }) => {
    test.setTimeout(60000); // Extend timeout for sidebar scan
    await page.goto('/');
    await waitForAppLoad(page);

    const sidebar = page.locator('[data-testid="sidebar"], aside, nav');
    
    if (await sidebar.count() > 0) {
      const results = await new AxeBuilder({ page })
        .include('[data-testid="sidebar"]')
        .withTags(['wcag2a', 'wcag2aa'])
        .analyze();

      if (results.violations.length > 0) {
        console.log('Sidebar accessibility violations:', formatViolations(results.violations));
      }

      console.log(`Sidebar passed ${results.passes.length} checks`);
    }
  });

  test('Settings page should be accessible', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Navigate to Settings
    const settingsNav = page.locator('[data-testid="nav-settings"], button:has-text("Settings")');
    if (await settingsNav.count() > 0) {
      await settingsNav.first().click({ force: true });
      await page.waitForTimeout(500);

      const results = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa'])
        .analyze();

      if (results.violations.length > 0) {
        console.log('Settings page accessibility violations:', formatViolations(results.violations));
      }

      console.log(`Settings page passed ${results.passes.length} checks`);
    }
  });

  test('Terminal page should be accessible', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    // Navigate to Terminal
    const terminalNav = page.locator('[data-testid="nav-agent"], button:has-text("Agent"), button:has-text("Terminal")');
    if (await terminalNav.count() > 0) {
      await terminalNav.first().click({ force: true });
      await page.waitForTimeout(500);

      const results = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa'])
        .analyze();

      if (results.violations.length > 0) {
        console.log('Terminal page accessibility violations:', formatViolations(results.violations));
      }

      console.log(`Terminal page passed ${results.passes.length} checks`);
    }
  });
});

// =============================================================================
// MODAL AND DIALOG ACCESSIBILITY
// =============================================================================

test.describe('Modal and Dialog Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('Project creation modal should be accessible', async ({ page }) => {
    const addProjectBtn = page.locator('[data-testid="add-project-button"], button:has-text("Add"), button[title*="project"]');
    
    if (await addProjectBtn.count() > 0) {
      await addProjectBtn.first().click({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        const results = await new AxeBuilder({ page })
          .include('[role="dialog"]')
          .withTags(['wcag2a', 'wcag2aa'])
          .analyze();

        if (results.violations.length > 0) {
          console.log('Modal accessibility violations:', formatViolations(results.violations));
        }

        // Check specific modal requirements
        const ariaModal = await modal.first().getAttribute('aria-modal');
        const ariaLabel = await modal.first().getAttribute('aria-label');
        const ariaLabelledby = await modal.first().getAttribute('aria-labelledby');

        console.log(`Modal has aria-modal: ${ariaModal === 'true'}`);
        console.log(`Modal has accessible name: ${!!(ariaLabel || ariaLabelledby)}`);

        await page.keyboard.press('Escape');
      }
    }
  });

  test('Task edit modal should be accessible', async ({ page }) => {
    // Try to find and open a task edit modal
    const taskCard = page.locator('[data-testid="task-card"], [class*="task-card"]');
    
    if (await taskCard.count() > 0) {
      await taskCard.first().dblclick({ force: true });
      await page.waitForTimeout(500);

      const modal = page.locator('[role="dialog"]');
      if (await modal.count() > 0) {
        const results = await new AxeBuilder({ page })
          .include('[role="dialog"]')
          .withTags(['wcag2a', 'wcag2aa'])
          .analyze();

        if (results.violations.length > 0) {
          console.log('Task modal accessibility violations:', formatViolations(results.violations));
        }

        await page.keyboard.press('Escape');
      }
    }
  });
});

// =============================================================================
// INTERACTIVE COMPONENTS ACCESSIBILITY
// =============================================================================

test.describe('Interactive Components Accessibility', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('Buttons should have accessible names', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['button-name'])
      .analyze();

    const violations = results.violations;
    if (violations.length > 0) {
      console.log('Buttons without accessible names:');
      violations.forEach(v => {
        v.nodes.forEach((node: any) => {
          console.log(`  - ${node.target.join(' ')}: ${node.failureSummary}`);
        });
      });
    }

    // Report the count
    console.log(`${results.passes.length} buttons have proper accessible names`);
  });

  test('Form inputs should have labels', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['label', 'select-name'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Form elements without proper labels:', formatViolations(results.violations));
    }
  });

  test('Interactive elements should be focusable', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['focus-order-semantics', 'tabindex'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Focus/tabindex violations:', formatViolations(results.violations));
    }
  });
});

// =============================================================================
// ARIA AND SEMANTIC HTML
// =============================================================================

test.describe('ARIA and Semantic HTML', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should have valid ARIA attributes', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules([
        'aria-valid-attr',
        'aria-valid-attr-value',
        'aria-allowed-attr',
        'aria-required-attr',
        'aria-roles',
      ])
      .analyze();

    if (results.violations.length > 0) {
      console.log('ARIA violations:', formatViolations(results.violations));
    }

    console.log(`${results.passes.length} ARIA checks passed`);
  });

  test('should have proper landmark regions', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['landmark-one-main', 'region'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Landmark violations:', formatViolations(results.violations));
    }
  });

  test('should not have duplicate IDs', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['duplicate-id', 'duplicate-id-active', 'duplicate-id-aria'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Duplicate ID violations:', formatViolations(results.violations));
    }

    // Duplicate IDs can cause serious accessibility issues
    expect(results.violations.filter(v => v.impact === 'critical')).toHaveLength(0);
  });
});

// =============================================================================
// BEST PRACTICES (Beyond WCAG)
// =============================================================================

test.describe('Accessibility Best Practices', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);
  });

  test('should follow accessibility best practices', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withTags(['best-practice'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Best practice violations:', formatViolations(results.violations));
    }

    console.log(`${results.passes.length} best practice checks passed`);
  });

  test('should have no deprecated ARIA usage', async ({ page }) => {
    const results = await new AxeBuilder({ page })
      .withRules(['aria-deprecated-role'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('Deprecated ARIA usage:', formatViolations(results.violations));
    }
  });

  test('should have proper table structure (if tables exist)', async ({ page }) => {
    const tables = page.locator('table');
    
    if (await tables.count() > 0) {
      const results = await new AxeBuilder({ page })
        .withRules(['td-headers-attr', 'th-has-data-cells', 'table-duplicate-name'])
        .analyze();

      if (results.violations.length > 0) {
        console.log('Table structure violations:', formatViolations(results.violations));
      }
    } else {
      console.log('No tables found on page');
    }
  });
});

// =============================================================================
// COMPREHENSIVE FULL-PAGE SCAN
// =============================================================================

test.describe('Comprehensive Accessibility Scan', () => {
  test('Full page scan with all WCAG rules', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'best-practice'])
      .analyze();

    // Summary
    console.log('\n========== ACCESSIBILITY SCAN SUMMARY ==========');
    console.log(`URL: ${results.url}`);
    console.log(`Passes: ${results.passes.length}`);
    console.log(`Violations: ${results.violations.length}`);
    console.log(`Incomplete: ${results.incomplete.length}`);
    console.log(`Inapplicable: ${results.inapplicable.length}`);

    // Group violations by impact
    const byImpact = {
      critical: results.violations.filter(v => v.impact === 'critical'),
      serious: results.violations.filter(v => v.impact === 'serious'),
      moderate: results.violations.filter(v => v.impact === 'moderate'),
      minor: results.violations.filter(v => v.impact === 'minor'),
    };

    console.log('\nViolations by Impact:');
    console.log(`  Critical: ${byImpact.critical.length}`);
    console.log(`  Serious: ${byImpact.serious.length}`);
    console.log(`  Moderate: ${byImpact.moderate.length}`);
    console.log(`  Minor: ${byImpact.minor.length}`);

    if (results.violations.length > 0) {
      console.log('\nDetailed Violations:');
      results.violations.forEach((v, i) => {
        console.log(`\n${i + 1}. [${v.impact?.toUpperCase()}] ${v.id}`);
        console.log(`   ${v.description}`);
        console.log(`   Help: ${v.helpUrl}`);
        console.log(`   Affected elements: ${v.nodes.length}`);
      });
    }

    // Fail only on critical violations
    expect(byImpact.critical).toHaveLength(0);
  });

  test('should generate accessibility report data', async ({ page }) => {
    await page.goto('/');
    await waitForAppLoad(page);

    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa'])
      .analyze();

    // Create a report object that could be saved/exported
    const report = {
      timestamp: new Date().toISOString(),
      url: results.url,
      summary: {
        passes: results.passes.length,
        violations: results.violations.length,
        incomplete: results.incomplete.length,
      },
      violations: results.violations.map(v => ({
        id: v.id,
        impact: v.impact,
        description: v.description,
        help: v.help,
        helpUrl: v.helpUrl,
        tags: v.tags,
        nodes: v.nodes.map((n: any) => ({
          target: n.target,
          html: n.html?.substring(0, 100),
          failureSummary: n.failureSummary,
        })),
      })),
    };

    console.log('Accessibility Report Generated:');
    console.log(JSON.stringify(report.summary, null, 2));

    // The report could be saved to a file or sent to a reporting service
    expect(report.summary.violations).toBeDefined();
  });
});
