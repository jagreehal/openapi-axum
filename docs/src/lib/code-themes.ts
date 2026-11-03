import { bundledThemes } from 'shiki/themes';

/** Night Owl, the base of Starlight's code themes, for the landing page's `<Code>` blocks. */
export const codeThemes = {
  dark: (await bundledThemes['night-owl']()).default,
  light: (await bundledThemes['night-owl-light']()).default,
};
