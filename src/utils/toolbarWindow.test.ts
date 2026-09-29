import { afterEach, describe, expect, it } from 'vitest'
import {
  TOOLBAR_PANEL_HEIGHT_COMPACT,
  cssToLogicalRatio,
  getCachedCssToLogicalRatio,
  getToolbarPanelHeight,
  rememberToolbarPanelHeight,
  resetToolbarPanelHeightCache,
} from './toolbarWindow'

describe('toolbar panel height cache', () => {
  afterEach(() => {
    resetToolbarPanelHeightCache()
  })

  it('defaults to measured compact height 234', () => {
    expect(TOOLBAR_PANEL_HEIGHT_COMPACT).toBe(234)
    expect(getToolbarPanelHeight()).toBe(234)
  })

  it('remembers measured heights for clamp/placement', () => {
    rememberToolbarPanelHeight(452.2)
    expect(getToolbarPanelHeight()).toBe(453)
  })
})

describe('cssToLogicalRatio', () => {
  it('is 1 when WebView DPR matches monitor scale (no text scaling)', () => {
    expect(cssToLogicalRatio(1, 1)).toBe(1)
    expect(cssToLogicalRatio(1.5, 1.5)).toBe(1)
    expect(cssToLogicalRatio(2, 2)).toBe(1)
    expect(cssToLogicalRatio(1.2500001, 1.25)).toBe(1)
  })

  it('returns the Windows text-scale factor folded into DPR', () => {
    expect(cssToLogicalRatio(1.5, 1)).toBeCloseTo(1.5)
    expect(cssToLogicalRatio(1.875, 1.25)).toBeCloseTo(1.5)
    expect(cssToLogicalRatio(2.25, 1)).toBeCloseTo(2.25)
  })

  it('ignores transient DPR lag after a monitor DPI change', () => {
    expect(cssToLogicalRatio(1, 1.5)).toBe(1)
  })

  it('falls back to 1 for invalid inputs', () => {
    expect(cssToLogicalRatio(0, 1)).toBe(1)
    expect(cssToLogicalRatio(1.5, 0)).toBe(1)
    expect(cssToLogicalRatio(Number.NaN, 1)).toBe(1)
  })

  it('defaults cached ratio to 1', () => {
    expect(getCachedCssToLogicalRatio()).toBe(1)
  })
})
