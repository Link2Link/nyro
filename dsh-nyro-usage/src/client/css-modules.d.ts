/** CSS modules are inlined by the tsdown plugin: importing a `.module.css`
 * yields a class-name map and injects the stylesheet once. */
declare module '*.module.css' {
  const css: Record<string, string>
  export default css
}
