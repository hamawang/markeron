import { describe, expect, it } from 'vitest'
import { resolveLaserMode } from './laserMode'

describe('laserMode', () => {
  it('defaults to trail', () => {
    expect(resolveLaserMode()).toBe('trail')
    expect(resolveLaserMode({})).toBe('trail')
  })

  it('reads explicit laserMode', () => {
    expect(resolveLaserMode({ laserMode: 'writing' })).toBe('writing')
    expect(resolveLaserMode({ laserMode: 'trail' })).toBe('trail')
  })
})
