/**
 * Module augmentation for plain `schemastery`: accepts the fork's
 * `volatile` meta flag (meta.volatile) so Config fields can carry
 * `.extra('volatile', true)` — exactly what `@deepseek-ai/schemastery`'s
 * volatile() does — while staying on the plain package.
 */
declare global {
  namespace Schemastery {
    interface Meta<T = any> {
      /** Marks a field live-editable through the settings surface without remounting the plugin fiber. */
      volatile?: boolean
    }
  }
}

export {}
