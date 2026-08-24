/**
 * Browser-half entry for the dsh-nyro-usage plugin — runs inside the dsh web
 * GUI.
 *
 * Registers the nyro-usage locale dictionaries and mounts the two DOM
 * surfaces: the sidebar entry row (toggles the panel) and the provider
 * usage panel in the center column; plus the plugin settings card over the
 * `nyro-usage` namespace. Failure policy: DOM mounting problems are logged,
 * never thrown — the web shell fails the whole boot when a plugin apply
 * throws, and an external plugin must not take the GUI down.
 *
 * Export discipline: the /client surface carries what cordis loading needs
 * plus types only — all value exports stay internal.
 */
import type { ClientContext, SettingsScope, SettingsScopeSpec } from '@deepseek-ai/dsh-client-runtime/client'
// Type-only: pulls the locale plugin's Context merge (ctx.locale).
import type {} from '@deepseek-ai/dsh-client-locale/client'
// Type-only: pulls the LocaleNamespaceMap merge table.
import type {} from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import type {} from '@deepseek-ai/dsh-client-ui-settings/client'
import { NyroPanelApi } from './api.ts'
import { en, zh, type NyroUsageKey } from './locales.ts'
import { mountPanel } from './mount.tsx'
import { NyroUsageSettingsCard, NyroUsageSettingsCardController, type NyroUsageSettings } from './NyroSettingsCard.tsx'
import { PanelController } from './controller.ts'
import { mountSidebarEntry } from './sidebar-entry.ts'
import { tt } from './tt.ts'

/** Locale namespace this plugin owns. */
const NS = 'nyro-usage'

/** Settings namespace the nyro-usage card edits (the Host plugin registers it). */
const NYRO_USAGE_NS = 'nyro-usage'

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** nyro-usage surface copy. */
    'nyro-usage': NyroUsageKey
  }

  interface SlotMap {
    /**
     * The child slot the Web UI plugin group declares; this card registers
     * into the group instead of the top-level `settings.plugin.item` list.
     * Spelled here with the same shape so this package can register without
     * depending on the sibling UI package.
     */
    'web-ui.plugin.item': { kind: 'list'; scope: 'root'; owner: NyroUsagePluginItemOwnerProps }
  }
}

/** Owner share of a plugin card (the section supplies nothing). */
export interface NyroUsagePluginItemOwnerProps {
  /** Marker field: card owner props are intentionally empty. */
  children?: never
}

declare module '@deepseek-ai/cordis' {
  interface Context {
    /**
     * Optional rc.6 compatibility binder provided by dsh-web-ui-settings;
     * absent when that group plugin is not installed, so callers fall back to
     * the official settings scope.
     */
    webUiSettings?: { bind<S>(spec: SettingsScopeSpec<S>): SettingsScope<S> }
  }
}

/** Required services (fiber inject waiting — the runtime must be up first). */
export const inject = ['slots', 'locale', 'connection', 'settingsScope', 'remote']

/** Type-only surface (export discipline: no value exports beyond the plugin contract). */
export type { PanelControllerSnapshot } from './controller.ts'
export type { NyroUsagePanelProps } from './panel/NyroUsagePanel.tsx'
export type { ProviderCardProps } from './panel/ProviderCard.tsx'
export type { TierBarProps } from './panel/TierBar.tsx'
export type { NyroUsageSettingsCardProps } from './NyroSettingsCard.tsx'
export type { NyroUsageKey } from './locales.ts'

/**
 * Mount the nyro usage surfaces.
 * @param ctx - client root context (locale + settings services).
 */
export function apply(ctx: ClientContext): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'nyro-usage: dictionaries')

  const controller = new PanelController()
  const api = new NyroPanelApi()
  const disposers: Array<() => void> = []
  try {
    disposers.push(mountSidebarEntry(controller, tt('entry.label'), tt('entry.tooltip')))
    disposers.push(mountPanel(controller, api))
  } catch (error) {
    console.error('[dsh-nyro-usage] panel mount failed', error)
  }
  ctx.effect(() => () => {
    for (const dispose of disposers.splice(0)) dispose()
  }, 'nyro-usage: surfaces')

  // Plugin configuration card: one staged form over the `nyro-usage`
  // settings namespace, contributed to the plugin-configuration section.
  const binder = ctx.get('webUiSettings') ?? ctx.settingsScope
  const settings = new NyroUsageSettingsCardController(
    binder.bind<NyroUsageSettings>({ namespace: NYRO_USAGE_NS }),
  )
  ctx.slots.inject('web-ui.plugin.item', () => ctx.slots.register({
    name: 'web-ui.plugin.item',
    id: 'nyro-usage',
    order: 120,
    locale: NS,
    inject: () => settings.inject(),
  }, NyroUsageSettingsCard))
}
