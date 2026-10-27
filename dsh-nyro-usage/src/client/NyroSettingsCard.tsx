/**
 * The nyro-usage settings card: the master switch, the primary gateway
 * (name + baseUrl + adminToken — the original single-gateway fields), the
 * list of additional nyro gateways, and the refresh parameters. Registers
 * into the `web-ui.plugin.item` slot the plugin-configuration section
 * renders, bound to the `nyro-usage` settings namespace. One Save writes the
 * scalar fields and the gateway-list edits in a single mutation. Includes a
 * "test connection" action that exercises every saved gateway through the
 * host proxy.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import type { InjectFace, PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type { SettingsScope, SnapshotStore } from '@deepseek-ai/dsh-client-runtime/client'
import { NyroPanelApi, type NyroStatusResponse, type NyroTestRouteResponse } from './api.ts'
import { InstancesForm, type InstanceField, type InstanceRowState, type InstancesState } from './instances-editor.ts'
import { BooleanField, PluginSettingsCard, ValueField } from './PluginSettingsCard.tsx'
import { CardForm, booleanField, numberField, secretField, textField, type CardActions, type CardShell, type FieldState as CardFieldState } from './settings-form.ts'
import css from './settings-card.module.css'

/** One additional gateway as stored in the namespace (token redacted on read-back). */
export interface NyroUsageInstanceSettings {
  name?: string
  baseUrl?: string
  adminToken?: string
  enabled?: boolean
}

/** The nyro-usage fields this card edits (the namespace's full schema). */
export interface NyroUsageSettings {
  enabled?: boolean
  name?: string
  baseUrl?: string
  adminToken?: string
  instances?: NyroUsageInstanceSettings[]
  refreshSeconds?: number
  cacheSeconds?: number
}

/** What the nyro-usage card renders. */
export interface NyroUsageSettingsCardState extends CardShell {
  enabled: CardFieldState
  name: CardFieldState
  baseUrl: CardFieldState
  adminToken: CardFieldState
  refreshSeconds: CardFieldState
  cacheSeconds: CardFieldState
  /** The additional gateways and whether they can be edited here. */
  instances: InstancesState
}

/** The gateway-list actions the card's slot entry injects. */
export interface NyroUsageInstanceActions {
  /** Stage one field of a gateway row (text, or a boolean for `enabled`). */
  editInstance: (key: string, field: InstanceField, value: string | boolean) => void
  /** Stage a new gateway row. */
  addInstance: () => void
  /** Stage a gateway row's removal. */
  removeInstance: (key: string) => void
}

/** The registration-side face the card's slot entry injects. */
export interface NyroUsageSettingsCardFace extends CardActions, NyroUsageInstanceActions {
  hooks: {
    /** Card snapshot bound by the renderer as useNyroUsageSettingsCard. */
    nyroUsageSettingsCard: SnapshotStore<NyroUsageSettingsCardState>
  }
}

/** Bridges the `nyro-usage` scope onto the card's staged form. */
export class NyroUsageSettingsCardController {
  private readonly instances: InstancesForm
  private readonly form: CardForm<NyroUsageSettings>
  private readonly store: SnapshotStore<NyroUsageSettingsCardState>

  /** @param scope - the bound settings scope for the `nyro-usage` namespace. */
  constructor(scope: SettingsScope<NyroUsageSettings>) {
    this.instances = new InstancesForm(scope)
    this.form = new CardForm(scope, [
      booleanField('enabled'),
      textField('name'),
      textField('baseUrl'),
      secretField('adminToken'),
      numberField('refreshSeconds', { integer: true, min: 15 }),
      numberField('cacheSeconds', { integer: true, min: 0 }),
    ], this.instances)
    this.store = this.form.bind(() => this.projection())
  }

  private projection(): NyroUsageSettingsCardState {
    return {
      ...this.form.shell(),
      enabled: this.form.field('enabled'),
      name: this.form.field('name'),
      baseUrl: this.form.field('baseUrl'),
      adminToken: this.form.field('adminToken'),
      refreshSeconds: this.form.field('refreshSeconds'),
      cacheSeconds: this.form.field('cacheSeconds'),
      instances: this.instances.state(),
    }
  }

  /**
   * Build the face the card's slot registration injects.
   * @returns the card's snapshot, its form actions and the gateway-list actions.
   */
  inject(): NyroUsageSettingsCardFace {
    return {
      hooks: { nyroUsageSettingsCard: this.store },
      ...this.form.actions(),
      editInstance: (key, field, value) => { this.instances.edit(key, field, value) },
      addInstance: () => { this.instances.add() },
      removeInstance: (key) => { this.instances.remove(key) },
    }
  }
}

/** Props the renderer binds for the nyro-usage card. */
export type NyroUsageSettingsCardProps =
  PropsRuntime<'web-ui.plugin.item'>
  & PropsLocale<'nyro-usage'>
  & InjectFace<NyroUsageSettingsCardFace>

/** Token presence read back from the host status route (never the token). */
type TokenFlags = NyroStatusResponse['tokens']

/** A DOM-id-safe form of a row key. */
function idPart(key: string): string {
  return key.replace(/[^A-Za-z0-9_-]/g, '-')
}

/** Props of one additional-gateway row. */
interface InstanceRowProps {
  row: InstanceRowState
  /** 1-based display position. */
  position: number
  /** Token stored for this row (undefined while the status is unknown). */
  hasToken: boolean | undefined
  disabled: boolean
  t: NyroUsageSettingsCardProps['t']
  onEdit: (field: InstanceField, value: string | boolean) => void
  onRemove: () => void
}

/** Render one additional-gateway row. */
function InstanceRow(props: InstanceRowProps) {
  const { row, t, disabled } = props
  const id = `settings-nyro-usage-instance-${idPart(row.key)}`
  const title = row.name.trim() !== '' ? row.name.trim() : t('settings.instanceUntitled', { index: props.position })
  const isNew = row.storedIndex === undefined
  const tokenPlaceholder = isNew
    ? 'nyro NYRO_ADMIN_TOKEN'
    : props.hasToken === true ? t('settings.instanceTokenSet') : t('settings.instanceTokenUnset')
  return (
    <li className={css.instanceRow} role="group" aria-labelledby={`${id}-title`}>
      <div className={css.instanceHead}>
        <h4 id={`${id}-title`} className={css.instanceTitle} title={title}>{title}</h4>
        {isNew ? <span className={css.badge}>{t('settings.instanceNew')}</span> : null}
        <label className={css.instanceToggle}>
          <input
            type="checkbox"
            checked={row.enabled}
            disabled={disabled}
            onChange={(event) => { props.onEdit('enabled', event.target.checked) }}
          />
          {t('settings.instanceEnabled')}
        </label>
        <button
          type="button"
          className={css.instanceRemove}
          disabled={disabled}
          aria-label={t('settings.instanceRemoveLabel', { name: title })}
          onClick={props.onRemove}
        >
          {t('settings.instanceRemove')}
        </button>
      </div>
      <div className={css.field}>
        <label className={css.label} htmlFor={`${id}-name`}>{t('settings.name')}</label>
        <input
          id={`${id}-name`}
          className={css.input}
          type="text"
          value={row.name}
          disabled={disabled}
          onChange={(event) => { props.onEdit('name', event.target.value) }}
        />
        <p className={css.hint}>{t('settings.nameHint')}</p>
      </div>
      <div className={css.field}>
        <label className={css.label} htmlFor={`${id}-url`}>{t('settings.baseUrl')}</label>
        <input
          id={`${id}-url`}
          className={row.badUrl ? css.inputInvalid : css.input}
          type="text"
          inputMode="url"
          value={row.baseUrl}
          placeholder="http://192.168.31.2:19531"
          disabled={disabled}
          {...row.badUrl ? { 'aria-invalid': true } : {}}
          onChange={(event) => { props.onEdit('baseUrl', event.target.value) }}
        />
        <p className={row.badUrl ? css.invalid : css.hint}>
          {row.badUrl ? t('settings.instanceBadUrl') : t('settings.baseUrlHint')}
        </p>
      </div>
      <div className={css.field}>
        <label className={css.label} htmlFor={`${id}-token`}>{t('settings.adminToken')}</label>
        <input
          id={`${id}-token`}
          className={row.tokenMissing ? css.inputInvalid : css.input}
          type="password"
          autoComplete="new-password"
          value={row.token}
          placeholder={tokenPlaceholder}
          disabled={disabled}
          {...row.tokenMissing ? { 'aria-invalid': true } : {}}
          onChange={(event) => { props.onEdit('token', event.target.value) }}
        />
        <p className={row.tokenMissing ? css.invalid : css.hint}>
          {row.tokenMissing ? t('settings.instanceTokenRequired') : t('settings.adminTokenHint')}
        </p>
      </div>
    </li>
  )
}

/** Render the nyro-usage card. */
export function NyroUsageSettingsCard(props: NyroUsageSettingsCardProps) {
  const { t } = props
  const state = props.useNyroUsageSettingsCard(snapshot => snapshot)
  const disabled = !state.writable
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<NyroTestRouteResponse | null>(null)
  const [tokens, setTokens] = useState<TokenFlags | undefined>(undefined)

  // Token presence comes from the host (the settings read-back redacts
  // secrets entirely); refreshed on mount and after every save.
  const loadTokens = useCallback(async (): Promise<void> => {
    try {
      setTokens((await new NyroPanelApi().status()).tokens)
    } catch {
      setTokens(undefined)
    }
  }, [])
  const wasSaving = useRef(false)
  useEffect(() => {
    if (wasSaving.current && !state.saving) void loadTokens()
    wasSaving.current = state.saving
  }, [state.saving, loadTokens])
  useEffect(() => { void loadTokens() }, [loadTokens])

  const runTest = async (): Promise<void> => {
    if (testing) return
    setTesting(true)
    setTestResult(null)
    try {
      setTestResult(await new NyroPanelApi().test())
    } catch (error) {
      setTestResult({ ok: false, kind: 'network', message: error instanceof Error ? error.message : String(error), instances: [] })
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
  const list = state.instances
  const listLocked = disabled || list.mode !== 'editable'
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

      <section className={css.group} aria-labelledby="settings-nyro-usage-primary">
        <h3 id="settings-nyro-usage-primary" className={css.groupTitle}>{t('settings.primary')}</h3>
        <p className={css.groupHint}>{t('settings.primaryHint')}</p>
        <ValueField
          id="settings-nyro-usage-name"
          label={t('settings.name')}
          hint={t('settings.nameHint')}
          {...fieldProps}
          {...state.name}
          onEdit={(text) => { props.edit('name', text) }}
          onReset={() => { props.resetField('name') }}
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
          placeholder={tokens?.primary === true ? t('settings.instanceTokenSet') : 'nyro NYRO_ADMIN_TOKEN'}
          {...fieldProps}
          {...state.adminToken}
          onEdit={(text) => { props.edit('adminToken', text) }}
          onReset={() => { props.resetField('adminToken') }}
        />
      </section>

      <section className={css.group} aria-labelledby="settings-nyro-usage-instances">
        <h3 id="settings-nyro-usage-instances" className={css.groupTitle}>{t('settings.instances')}</h3>
        <p className={css.groupHint}>{t('settings.instancesHint')}</p>
        {list.mode === 'inherited' ? <p className={css.instanceNote} role="status">{t('settings.instancesInherited')}</p> : null}
        {list.rows.length > 0
          ? (
            <ul className={css.instanceList}>
              {list.rows.map((row, index) => (
                <InstanceRow
                  key={row.key}
                  row={row}
                  position={index + 1}
                  hasToken={row.storedIndex === undefined ? undefined : tokens?.rows[row.storedIndex]}
                  disabled={listLocked}
                  t={t}
                  onEdit={(field, value) => { props.editInstance(row.key, field, value) }}
                  onRemove={() => { props.removeInstance(row.key) }}
                />
              ))}
            </ul>
          )
          : <p className={css.instanceNote}>{t('settings.instancesEmpty')}</p>}
        {list.mode === 'editable'
          ? (
            <button type="button" className={css.instanceAdd} disabled={disabled} onClick={props.addInstance}>
              + {t('settings.instanceAdd')}
            </button>
          )
          : null}
      </section>

      <section className={css.group} aria-label={t('settings.refreshSeconds')}>
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
      </section>

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
              ? <span className={css.testOk} role="status">{t('settings.testOk', { count: testResult.providerCount ?? 0 })}</span>
              : <span className={css.testFail} role="status">{t('settings.testFail', { message: testResult.message })}</span>
          )
          : null}
        {testResult !== null && testResult.instances.length > 1
          ? (
            <ul className={css.testList}>
              {testResult.instances.map(result => (
                <li key={result.id} className={result.ok ? css.testOk : css.testFail}>
                  {result.ok
                    ? t('settings.testInstanceOk', { name: result.name, count: result.providerCount ?? 0 })
                    : t('settings.testInstanceFail', { name: result.name, message: result.message })}
                </li>
              ))}
            </ul>
          )
          : null}
      </div>
    </PluginSettingsCard>
  )
}
