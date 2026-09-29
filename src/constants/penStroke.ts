import getStroke, {
  getStrokeOutlinePoints,
  getStrokePoints,
  type StrokeOptions,
  type StrokePoint,
} from 'perfect-freehand'
import type { Point } from '../composables/drawingTypes'
import type { StrokeSmoothing } from '../utils/strokeSmoothing'

/** Base min distance in device pixels (squared) before adaptive area scaling. */
export const BASE_MIN_DIST_DEVICE_SQ = 4

/** Viewport area used as the 1.0 reference for adaptive sampling. */
export const BASE_VIEWPORT_AREA = 1440 * 900

/** Cap how aggressively large CSS viewports thin points (was 4 → ~4 CSS px). */
export const MAX_AREA_SCALE = 2

export interface PenStrokeStyle {
  /** Position streamline applied before points are stored (0 = off). */
  inputStreamline: number
  thinning: number
  smoothing: number
  /** Extra streamline inside perfect-freehand (kept low; we pre-smooth). */
  strokeStreamline: number
}

const STYLE_BY_LEVEL: Record<StrokeSmoothing, PenStrokeStyle> = {
  off: {
    inputStreamline: 0,
    thinning: 0.25,
    smoothing: 0.35,
    strokeStreamline: 0,
  },
  standard: {
    inputStreamline: 0.35,
    thinning: 0.55,
    smoothing: 0.45,
    strokeStreamline: 0.1,
  },
  strong: {
    inputStreamline: 0.5,
    thinning: 0.65,
    smoothing: 0.55,
    strokeStreamline: 0.15,
  },
}

export function penStrokeStyle(level: StrokeSmoothing): PenStrokeStyle {
  return STYLE_BY_LEVEL[level]
}

/**
 * Min squared distance in CSS pixels so physical spacing stays near
 * ~2 device px (scaled gently on large viewports).
 */
export function computeMinDistSq(cssW: number, cssH: number, dpr: number): number {
  const area = cssW * cssH
  const scale = area / BASE_VIEWPORT_AREA
  const deviceDistSq = scale > 1.5 ? BASE_MIN_DIST_DEVICE_SQ * Math.min(scale, MAX_AREA_SCALE) : BASE_MIN_DIST_DEVICE_SQ
  const safeDpr = dpr > 0 ? dpr : 1
  return deviceDistSq / (safeDpr * safeDpr)
}

/** Clamp pointer pressure for ink; mouse / missing → 0.5. */
export function normalizePressure(pressure: number | undefined | null): number {
  if (pressure == null || !(pressure > 0)) return 0.5
  return Math.min(1, Math.max(0.05, pressure))
}

/** Real pressure devices: stylus / finger. Mouse stays uniform-width ink. */
export function usesPressureInk(pointerType: string | undefined | null): boolean {
  return pointerType === 'pen' || pointerType === 'touch'
}

/**
 * Smooth one sample toward the previous stored point (Excalidraw-style).
 * Pressure is taken from the raw sample (not blended).
 */
export function smoothPenPoint(prev: Point, raw: Point, streamline: number): Point {
  if (streamline <= 0) return raw
  const pull = 1 - streamline
  return {
    x: prev.x + (raw.x - prev.x) * pull,
    y: prev.y + (raw.y - prev.y) * pull,
    pressure: raw.pressure,
  }
}

export type FreehandInputPoint = [number, number, number]

export function toFreehandInputs(points: Point[]): FreehandInputPoint[] {
  const out: FreehandInputPoint[] = []
  for (let i = 0; i < points.length; i++) {
    const p = points[i]
    out.push([p.x, p.y, normalizePressure(p.pressure)])
  }
  return out
}

export function inkFreehandOptions(
  size: number,
  level: StrokeSmoothing,
  last: boolean,
  pointerType?: string | null,
): StrokeOptions {
  const style = penStrokeStyle(level)
  const pressureInk = usesPressureInk(pointerType)
  return {
    size: Math.max(1, size),
    // Mouse: no thinning (uniform). Pen/touch: real pressure only — never speed-simulate.
    thinning: pressureInk ? style.thinning : 0,
    smoothing: style.smoothing,
    streamline: style.strokeStreamline,
    simulatePressure: false,
    easing: (t) => t,
    start: { taper: 0, cap: true },
    end: { taper: 0, cap: true },
    last,
  }
}

export function getInkOutline(
  points: Point[],
  size: number,
  level: StrokeSmoothing,
  last: boolean,
  pointerType?: string | null,
): number[][] {
  if (points.length === 0) return []
  return getStroke(toFreehandInputs(points), inkFreehandOptions(size, level, last, pointerType))
}

/** Outline for a contiguous run of stroke points (e.g. one slice of a live stroke). */
export function getInkOutlineFromStrokePoints(strokePoints: StrokePoint[], options: StrokeOptions): number[][] {
  if (strokePoints.length === 0) return []
  return getStrokeOutlinePoints(strokePoints, options)
}

function isValidPressure(pressure: number | null | undefined): pressure is number {
  return pressure != null && pressure >= 0
}

/**
 * Append-only equivalent of perfect-freehand's `getStrokePoints` with `last: false`,
 * so a live stroke does not re-run the O(n) streamline pass every frame.
 * `slice(0)` must deep-equal `getStrokePoints(inputs, options)` (see penStroke.test.ts).
 * Indices below `settledCount` never change once emitted.
 */
export class InkStrokePointStream {
  private readonly size: number
  private readonly lerpT: number
  private readonly options: StrokeOptions
  private readonly settled: StrokePoint[] = []
  private firstInput: FreehandInputPoint | null = null
  private pendingInput: FreehandInputPoint | null = null
  private inputCount = 0
  private runningLength = 0
  private started = false

  constructor(options: StrokeOptions) {
    this.options = { ...options, last: false }
    this.size = options.size ?? 16
    this.lerpT = 0.15 + (1 - (options.streamline ?? 0.5)) * 0.85
  }

  get settledCount(): number {
    return this.settled.length
  }

  settledAt(index: number): StrokePoint {
    return this.settled[index]
  }

  append(input: FreehandInputPoint) {
    if (this.inputCount === 0) {
      this.firstInput = input
      this.settled.push({
        point: [input[0], input[1]],
        pressure: isValidPressure(input[2]) ? input[2] : 0.25,
        vector: [1, 1],
        distance: 0,
        runningLength: 0,
      })
    } else {
      if (this.pendingInput) this.settle(this.pendingInput)
      this.pendingInput = input
    }
    this.inputCount++
  }

  /**
   * Stroke points from `start` to the live end, or to settled index `end` (exclusive)
   * when given. Fresh array; entries are shared.
   */
  slice(start: number, end?: number): StrokePoint[] {
    if (this.inputCount <= 2) {
      if (!this.firstInput) return []
      const inputs = this.pendingInput ? [this.firstInput, this.pendingInput] : [this.firstInput]
      return getStrokePoints(inputs, this.options).slice(start, end)
    }

    const out = this.settled.slice(start, end)
    const tip = end === undefined && this.pendingInput ? this.nextPoint(this.pendingInput, true) : null
    if (tip) out.push(tip)
    if (start === 0 && out.length > 0) {
      out[0] = { ...out[0], vector: out[1]?.vector || [0, 0] }
    }
    return out
  }

  private settle(input: FreehandInputPoint) {
    const next = this.nextPoint(input, false)
    if (next) this.settled.push(next)
  }

  /** Mirrors one iteration of perfect-freehand's getStrokePoints loop (float-for-float). */
  private nextPoint(input: FreehandInputPoint, isTip: boolean): StrokePoint | null {
    const prev = this.settled[this.settled.length - 1]
    const px = prev.point[0]
    const py = prev.point[1]
    const x = px + (input[0] - px) * this.lerpT
    const y = py + (input[1] - py) * this.lerpT
    if (px === x && py === y) return null

    const distance = Math.hypot(y - py, x - px)
    const runningLength = this.runningLength + distance
    if (!isTip) {
      this.runningLength = runningLength
      if (!this.started) {
        if (runningLength < this.size) return null
        this.started = true
      }
    }

    const vx = px - x
    const vy = py - y
    const len = Math.hypot(vx, vy)
    return {
      point: [x, y],
      pressure: isValidPressure(input[2]) ? input[2] : 0.5,
      vector: [vx / len, vy / len],
      distance,
      runningLength,
    }
  }
}

/** Settled stroke points required before a live-ink chunk may be frozen. */
export const INK_FREEZE_MIN_POINTS = 64
/** Settled points kept re-outlined each frame behind the live tip. */
export const INK_LIVE_TAIL_POINTS = 32

/**
 * Path length shared by a frozen chunk and the next slice. Must exceed the stroke
 * diameter plus perfect-freehand's 3px end skip so each slice's caps sit inside
 * its neighbour's body.
 */
export function inkChunkOverlapLength(size: number): number {
  return Math.max(1, size) * 2 + 4
}

/**
 * Next live-ink chunk to freeze: stroke points [chunkStart, end] get rasterized once;
 * the live tail then restarts at `nextStart` (overlapping by `inkChunkOverlapLength`).
 */
export function planInkFreeze(
  stream: InkStrokePointStream,
  chunkStart: number,
  size: number,
): { end: number; nextStart: number } | null {
  const end = stream.settledCount - 1 - INK_LIVE_TAIL_POINTS
  if (end - chunkStart < INK_FREEZE_MIN_POINTS) return null

  const overlap = inkChunkOverlapLength(size)
  const endLength = stream.settledAt(end).runningLength
  let nextStart = end
  while (nextStart > chunkStart + 1 && endLength - stream.settledAt(nextStart).runningLength < overlap) {
    nextStart--
  }
  if (endLength - stream.settledAt(nextStart).runningLength < overlap) return null
  return { end, nextStart }
}

/** Build a closed Path2D from a perfect-freehand outline. */
export function outlineToPath2D(outline: number[][]): Path2D {
  const path = new Path2D()
  if (outline.length < 2) {
    if (outline.length === 1) {
      path.moveTo(outline[0][0], outline[0][1])
      path.arc(outline[0][0], outline[0][1], 0.5, 0, Math.PI * 2)
    }
    return path
  }

  path.moveTo(outline[0][0], outline[0][1])
  for (let i = 1; i < outline.length - 1; i++) {
    const [x0, y0] = outline[i]
    const [x1, y1] = outline[i + 1]
    path.quadraticCurveTo(x0, y0, (x0 + x1) / 2, (y0 + y1) / 2)
  }
  const last = outline[outline.length - 1]
  const first = outline[0]
  path.quadraticCurveTo(last[0], last[1], (last[0] + first[0]) / 2, (last[1] + first[1]) / 2)
  path.closePath()
  return path
}
