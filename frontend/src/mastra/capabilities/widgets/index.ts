import { exchangeUrl } from '../shared/exchange'
import { defineCapability } from '../types'
import { WIDGETS_INSTRUCTIONS, WIDGETS_UNAVAILABLE } from './instructions'
import { createWidgetTools } from './tools'

/**
 * The show tools: each returns a validated widget spec the browser binds to live data.
 * Always enabled, since content widgets need no exchange; live kinds refuse without one.
 */
export const widgets = defineCapability({
  id: 'widgets',
  enabled: () => true,
  instructions: WIDGETS_INSTRUCTIONS,
  unavailable: WIDGETS_UNAVAILABLE,
  create: env => createWidgetTools(exchangeUrl(env)),
})
