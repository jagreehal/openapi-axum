import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://jagreehal.github.io',
  base: '/openapi-axum',
  integrations: [
    starlight({
      title: 'openapi-axum',
      description: 'OpenAPI for axum, derived from your handler signatures and enforced at runtime.',
      // `404.mdx` is the only `/404` route.
      disable404Route: true,
      favicon: '/favicon.svg',
      editLink: { baseUrl: 'https://github.com/jagreehal/openapi-axum/edit/main/docs/' },
      components: {
        PageTitle: './src/components/PageTitle.astro',
        ThemeProvider: './src/components/ThemeProvider.astro',
        ThemeSelect: './src/components/ThemeSelect.astro',
      },
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/jagreehal/openapi-axum' },
      ],
      customCss: [
        '@fontsource-variable/inter',
        '@fontsource/jetbrains-mono',
        '@fontsource/jetbrains-mono/500.css',
        '@fontsource/jetbrains-mono/600.css',
        '@fontsource/jetbrains-mono/700.css',
        './src/styles/custom.css',
      ],
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', slug: 'introduction' },
            { label: 'Install', slug: 'install' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'How the document is built', slug: 'guides/document' },
            { label: 'Responses', slug: 'guides/responses' },
            { label: 'Request validation', slug: 'guides/validation' },
            { label: 'Custom extractors', slug: 'guides/extractors' },
            { label: 'Routers', slug: 'guides/routers' },
            { label: 'CI: contract checks', slug: 'guides/ci' },
          ],
        },
        {
          label: 'Testing',
          items: [
            { label: 'Conformance', slug: 'testing/conformance' },
            { label: 'Runtime checks', slug: 'testing/runtime' },
            { label: 'Fuzzing with Schemathesis', slug: 'testing/schemathesis' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'What gets checked', slug: 'reference/guarantees' },
            { label: 'Performance', slug: 'reference/performance' },
            { label: 'Features and limits', slug: 'reference/features' },
            { label: 'Agent skill', slug: 'reference/agent-skill' },
          ],
        },
      ],
    }),
  ],
});
