/** `trail`: comet that decays while drawing. `writing`: strokes stay until the pen rests, then fade together. */
export type LaserMode = 'trail' | 'writing'

export const LASER_MODE_OPTIONS: LaserMode[] = ['trail', 'writing']

export function resolveLaserMode(general?: { laserMode?: LaserMode }): LaserMode {
  return general?.laserMode === 'writing' ? 'writing' : 'trail'
}
