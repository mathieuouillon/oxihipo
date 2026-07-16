// @ts-check
import { themes as prismThemes } from 'prism-react-renderer';

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'oxihipo',
  tagline:
    'Fast, pure-Rust reader and writer for HIPO v6 — with an uproot-shaped Python binding',
  favicon: 'img/favicon.ico',

  // Published to GitHub Pages at https://mathieuouillon.github.io/oxihipo/
  url: 'https://mathieuouillon.github.io',
  baseUrl: '/oxihipo/',
  organizationName: 'mathieuouillon',
  projectName: 'oxihipo',
  trailingSlash: false,

  // A broken link is a build failure — CI catches them before they ship.
  onBrokenLinks: 'throw',
  markdown: {
    // Explicit rather than relying on the default, which v4 is set to change.
    format: 'mdx',
    hooks: { onBrokenMarkdownLinks: 'throw' },
  },

  // NOTE: do not set `future: {v4: true}` here. It opts into every v4 behaviour
  // at once (including mdx1CompatDisabledByDefault) and silently renders every
  // `:::note` / `:::tip` admonition as literal ":::" text — the build still
  // succeeds, so it fails quietly. Verified on Docusaurus 3.10.2.

  i18n: { defaultLocale: 'en', locales: ['en'] },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          sidebarPath: './sidebars.js',
          editUrl: 'https://github.com/mathieuouillon/oxihipo/tree/main/website/',
        },
        blog: false, // library docs, not a blog
        theme: { customCss: './src/css/custom.css' },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      colorMode: { respectPrefersColorScheme: true },
      navbar: {
        title: 'oxihipo',
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docsSidebar',
            position: 'left',
            label: 'Docs',
          },
          { to: '/docs/rust/reading', label: 'Rust', position: 'left' },
          { to: '/docs/python/reading', label: 'Python', position: 'left' },
          {
            to: '/docs/performance/benchmarks',
            label: 'Benchmarks',
            position: 'left',
          },
          {
            href: 'https://github.com/mathieuouillon/oxihipo',
            label: 'GitHub',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Docs',
            items: [
              { label: 'Introduction', to: '/docs/intro' },
              { label: 'Getting started', to: '/docs/getting-started/rust' },
              { label: 'Rust guide', to: '/docs/rust/reading' },
              { label: 'Python guide', to: '/docs/python/reading' },
            ],
          },
          {
            title: 'Performance',
            items: [
              { label: 'Compression formats', to: '/docs/performance/compression' },
              { label: 'Benchmarks', to: '/docs/performance/benchmarks' },
              {
                label: 'Shared filesystems',
                to: '/docs/performance/shared-filesystems',
              },
            ],
          },
          {
            title: 'More',
            items: [
              {
                label: 'GitHub',
                href: 'https://github.com/mathieuouillon/oxihipo',
              },
              { label: 'Design notes', to: '/docs/design/python-binding' },
            ],
          },
        ],
        copyright: `Copyright © ${new Date().getFullYear()} Mathieu Ouillon. Built with Docusaurus. MIT licensed.`,
      },
      prism: {
        theme: prismThemes.github,
        darkTheme: prismThemes.dracula,
        additionalLanguages: ['rust', 'python', 'toml', 'bash'],
      },
    }),
};

export default config;
