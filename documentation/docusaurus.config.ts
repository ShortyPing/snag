import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

const config: Config = {
  title: 'Snag',
  tagline: 'A regression test runner for HTTP APIs. TOML suites, Rhai assertions.',
  favicon: 'img/favicon.svg',

  future: {
    v4: true,
  },

  url: 'https://shortyping.github.io',
  baseUrl: '/snag/',

  organizationName: 'ShortyPing',
  projectName: 'snag',

  onBrokenLinks: 'throw',

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
          routeBasePath: 'docs',
          editUrl: 'https://github.com/ShortyPing/snag/tree/main/documentation/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    image: 'img/logo.svg',
    colorMode: {
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'Snag',
      logo: {
        alt: 'Snag logo',
        src: 'img/logo.svg',
      },
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docsSidebar',
          position: 'left',
          label: 'Documentation',
        },
        {
          to: '/docs/reference/cli',
          label: 'CLI reference',
          position: 'left',
        },
        {
          to: '/docs/internals/architecture',
          label: 'Internals',
          position: 'left',
        },
        {
          href: 'https://github.com/ShortyPing/snag',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Learn',
          items: [
            {label: 'Introduction', to: '/docs/intro'},
            {label: 'Quickstart', to: '/docs/getting-started/quickstart'},
            {label: 'Your first suite', to: '/docs/getting-started/your-first-suite'},
          ],
        },
        {
          title: 'Reference',
          items: [
            {label: 'CLI', to: '/docs/reference/cli'},
            {label: 'Suite manifest', to: '/docs/reference/manifest'},
            {label: 'Script API', to: '/docs/reference/script-api'},
            {label: 'Report formats', to: '/docs/reference/report-formats'},
          ],
        },
        {
          title: 'Develop',
          items: [
            {label: 'Architecture', to: '/docs/internals/architecture'},
            {label: 'Module tour', to: '/docs/internals/module-tour'},
            {label: 'Contributing', to: '/docs/internals/contributing'},
            {label: 'GitHub', href: 'https://github.com/ShortyPing/snag'},
          ],
        },
      ],
      copyright: `Snag — built with Rust and Docusaurus. © ${new Date().getFullYear()}`,
    },
    prism: {
      theme: prismThemes.oneLight,
      darkTheme: prismThemes.oneDark,
      additionalLanguages: ['toml', 'bash', 'json', 'rust', 'yaml', 'diff'],
    },
    docs: {
      sidebar: {
        hideable: true,
      },
    },
    tableOfContents: {
      minHeadingLevel: 2,
      maxHeadingLevel: 4,
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
