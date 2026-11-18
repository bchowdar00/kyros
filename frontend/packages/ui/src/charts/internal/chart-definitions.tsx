import type { RefObject } from 'react'
import { useChartGeometry } from './chart-context'

interface ChartDefinitionsProps {
  plotClipId: string
  revealClipId: string
  revealRectRef: RefObject<SVGRectElement | null>
}

export function ChartDefinitions({
  plotClipId,
  revealClipId,
  revealRectRef,
}: ChartDefinitionsProps) {
  const { glowFilterId, height, width, plot } = useChartGeometry()
  return (
    <defs>
      <filter
        colorInterpolationFilters="sRGB"
        filterUnits="userSpaceOnUse"
        height={height + 40}
        id={glowFilterId}
        width={width + 40}
        x={-20}
        y={-20}
      >
        <feGaussianBlur in="SourceGraphic" result="tight" stdDeviation="1.4" />
        <feGaussianBlur in="SourceGraphic" result="wide" stdDeviation="4.5" />
        <feMerge>
          <feMergeNode in="wide" />
          <feMergeNode in="tight" />
        </feMerge>
      </filter>
      <clipPath id={revealClipId}>
        <rect height={height + 40} ref={revealRectRef} width={0} x={-20} y={-20} />
      </clipPath>
      <clipPath id={plotClipId}>
        <rect height={height + 40} width={plot.width + 12} x={plot.left - 6} y={-20} />
      </clipPath>
    </defs>
  )
}
