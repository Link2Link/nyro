/**
 * The nyro-usage settings card: the connection (baseUrl + adminToken) and
 * refresh parameters. Registers into the `web-ui.plugin.item` slot the
 * plugin-configuration section renders, bound to the `nyro-usage` settings
 * namespace. Includes a "test connection" action that exercises the saved
 * configuration through the host proxy.
 */

import { useState } from 'react'
import type { InjectFace, PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type { SettingsScope, SnapshotStore } from '@deepseek-ai/dsh-client-runtime/client'
import { NyroPanelApi, type NyroTestResult } from './api.ts'
import { BooleanField, PluginSettingsCard, ValueField } from './PluginSettingsCard.tsx'
import { CardForm, booleanField, numberField, secretField, textField, type CardActions, type CardShell, type FieldState as CardFieldState } from './settings-form.ts'
import css from './settings-card.module.css'
import { tt } from './tt.ts'

/** The nyro-usage fields this card edits (the namespace's full schema). */
export interface NyroUsageSettings {
  enabled?: boolean
  baseUrl?: string
  adminToken?: string
  refreshSeconds?: number
  cacheSeconds?: number
}

/** What the nyro-usage card renders. */
export interface NyroUsageSettingsCardState extends CardShell {
  enabled: CardFieldState
  baseUrl: CardFieldState
  adminToken: CardFieldState
  refreshSeconds: CardFieldState
  cacheSeconds: CardFieldState
}

/** The registration-side face the card's slot entry injects. */
export interface NyroUsageSettingsCardFace extends CardActions {
  hooks: {
    /** Card snapshot bound by the renderer as useNyroUsageSettingsCard. */
    nyroUsageSettingsCard: SnapshotStore<NyroUsageSettingsCardState>
  }
}

/** Bridges the `nyro-usage` scope onto the card's staged form. */
export class NyroUsageSettingsCardController {
  private readonly form: CardForm<NyroUsageSettings>
  private readonly store: SnapshotStore<NyroUsageSettingsCardState>

  /** @param scope - the bound settings scope for the `nyro-usage` namespace. */
  constructor(scope: SettingsScope<NyroUsageSettings>) {
    this.form = new CardForm(scope, [
      booleanField('enabled'),
      textField('baseUrl'),
      secretField('adminToken'),
      numberField('refreshSeconds', { integer: true, min: 15 }),
      numberField('cacheSeconds', { integer: true, min: 0 }),
    ])
    this.store = this.form.bind(() => this.projection())
  }

  private projection(): NyroUsageSettingsCardState {
    return {
      ...this.form.shell(),
      enabled: this.form.field('enabled'),
      baseUrl: this.form.field('baseUrl'),
      adminToken: this.form.field('adminToken'),
      refreshSeconds: this.form.field('refreshSeconds'),
      cacheSeconds: this.form.field('cacheSeconds'),
    }
  }

  /**
   * Build the face the card's slot registration injects.
   * @returns the card's snapshot and its form actions.
   */
  inject(): NyroUsageSettingsCardFace {
    return { hooks: { nyroUsageSettingsCard: this.store }, ...this.form.actions() }
  }
}

/** Props the renderer binds for the nyro-usage card. */
export type NyroUsageSettingsCardProps =
  PropsRuntime<'web-ui.plugin.item'>
  & PropsLocale<'nyro-usage'>
  & InjectFace<NyroUsageSettingsCardFace>

/** Render the nyro-usage card. */
export function NyroUsageSettingsCard(props: NyroUsageSettingsCardProps) {
  const { t } = props
  const state = props.useNyroUsageSettingsCard(snapshot => snapshot)
  const disabled = !state.writable
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<NyroTestResult | null>(null)

  const runTest = async (): Promise<void> => {
    if (testing) return
    setTesting(true)
    setTestResult(null)
    try {
      setTestResult(await new NyroPanelApi().test())
    } catch (error) {
      setTestResult({ ok: false, kind: 'network', message: error instanceof Error ? error.message : String(error) })
    } finally {
      setTesting(false)
    }
  }

  const fieldProps = {
    overriddenLabel: t('settings.overridden'),
    resetLabel: t('settings.reset'),
    invalidLabel: t('settings.invalidNumber'),
    disabled,
  }
  return (
    <PluginSettingsCard
      t={t}
      titleKey="settings.title"
      descriptionKey="settings.description"
      state={state}
      onSave={props.save}
      onDiscard={props.discard}
    >
      <BooleanField
        id="settings-nyro-usage-enabled"
        label={t('settings.enabled')}
        hint={t('settings.enabledHint')}
        inheritLabel={t('settings.inherit')}
        onLabel={t('settings.on')}
        offLabel={t('settings.off')}
        {...fieldProps}
        {...state.enabled}
        onEdit={(text) => { props.edit('enabled', text) }}
        onReset={() => { props.resetField('enabled') }}
      />
      <ValueField
        id="settings-nyro-usage-base-url"
        label={t('settings.baseUrl')}
        hint={t('settings.baseUrlHint')}
        placeholder="http://192.168.31.2:19531"
        {...fieldProps}
        {...state.baseUrl}
        onEdit={(text) => { props.edit('baseUrl', text) }}
        onReset={() => { props.resetField('baseUrl') }}
      />
      <ValueField
        id="settings-nyro-usage-admin-token"
        label={t('settings.adminToken')}
        hint={t('settings.adminTokenHint')}
        placeholder="nyro NYRO_ADMIN_TOKEN"
        {...fieldProps}
        {...state.adminToken}
        onEdit={(text) => { props.edit('adminToken', text) }}
        onReset={() => { props.resetField('adminToken') }}
      />
      <ValueField
        id="settings-nyro-usage-refresh"
        label={t('settings.refreshSeconds')}
        hint={t('settings.refreshSecondsHint')}
        numeric
        {...fieldProps}
        {...state.refreshSeconds}
        onEdit={(text) => { props.edit('refreshSeconds', text) }}
        onReset={() => { props.resetField('refreshSeconds') }}
      />
      <ValueField
        id="settings-nyro-usage-cache"
        label={t('settings.cacheSeconds')}
        hint={t('settings.cacheSecondsHint')}
        numeric
        {...fieldProps}
        {...state.cacheSeconds}
        onEdit={(text) => { props.edit('cacheSeconds', text) }}
        onReset={() => { props.resetField('cacheSeconds') }}
      />
      <div className={css.testRow}>
        <button
          type="button"
          className={css.testButton}
          disabled={testing || state.dirty || state.invalid}
          onClick={() => { void runTest() }}
        >
          {testing ? t('settings.testing') : t('settings.test')}
        </button>
        {testResult !== null
          ? (
            testResult.ok
              ? <span className={css.testOk}>{tt('settings.testOk', { count: testResult.providerCount ?? 0 })}</span>
              : <span className={css.testFail}>{tt('settings.testFail', { message: testResult.message })}</span>
          )
          : null}
      </div>
    </PluginSettingsCard>
  )
}
