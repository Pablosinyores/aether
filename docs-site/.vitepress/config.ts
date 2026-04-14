import { defineConfig } from 'vitepress'
import { withMermaid } from 'vitepress-plugin-mermaid'

export default withMermaid(
  defineConfig({
    title: 'Aether',
    description: 'Production-grade cross-DEX arbitrage engine for Ethereum Mainnet',

    head: [
      ['link', { rel: 'preconnect', href: 'https://fonts.googleapis.com' }],
      ['link', { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: '' }],
      ['link', { href: 'https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap', rel: 'stylesheet' }],
    ],

    themeConfig: {
      logo: '/logo.svg',
      siteTitle: 'Aether',

      nav: [
        { text: 'Guide', link: '/guide/introduction' },
        { text: 'Architecture', link: '/architecture/overview' },
        { text: 'Operations', link: '/operations/deployment' },
        { text: 'Development', link: '/development/contributing' },
        { text: 'Reference', link: '/reference/configuration' },
      ],

      sidebar: {
        '/guide/': [
          {
            text: 'Guide',
            items: [
              { text: 'Introduction', link: '/guide/introduction' },
              { text: 'How It Works', link: '/guide/how-it-works' },
              { text: 'Getting Started', link: '/guide/getting-started' },
              { text: 'Configuration', link: '/guide/configuration' },
            ],
          },
        ],
        '/architecture/': [
          {
            text: 'Architecture',
            items: [
              { text: 'Overview', link: '/architecture/overview' },
              { text: 'Rust Core', link: '/architecture/rust-core' },
              { text: 'Go Services', link: '/architecture/go-services' },
              { text: 'Smart Contract', link: '/architecture/smart-contract' },
              { text: 'gRPC Protocol', link: '/architecture/grpc-protocol' },
              { text: 'Design Decisions', link: '/architecture/design-decisions' },
            ],
          },
        ],
        '/operations/': [
          {
            text: 'Operations',
            items: [
              { text: 'Deployment', link: '/operations/deployment' },
              { text: 'Runbook', link: '/operations/runbook' },
              { text: 'Incident Response', link: '/operations/incident-response' },
              { text: 'Monitoring', link: '/operations/monitoring' },
            ],
          },
        ],
        '/development/': [
          {
            text: 'Development',
            items: [
              { text: 'Contributing', link: '/development/contributing' },
              { text: 'Adding a DEX', link: '/development/adding-a-dex' },
              { text: 'Testing', link: '/development/testing' },
              { text: 'Tooling & Scripts', link: '/development/tooling' },
            ],
          },
        ],
        '/reference/': [
          {
            text: 'Reference',
            items: [
              { text: 'Configuration', link: '/reference/configuration' },
              { text: 'Metrics', link: '/reference/metrics' },
              { text: 'Risk Parameters', link: '/reference/risk-parameters' },
              { text: 'gRPC API', link: '/reference/api' },
            ],
          },
        ],
      },

      socialLinks: [
        { icon: 'github', link: 'https://github.com/aether-arb/aether' },
      ],

      search: {
        provider: 'local',
      },

      editLink: {
        pattern: 'https://github.com/aether-arb/aether/edit/main/docs-site/:path',
        text: 'Edit this page on GitHub',
      },

      footer: {
        message: 'Released under the MIT License.',
        copyright: 'Copyright 2024-present Aether Contributors',
      },
    },

    mermaid: {
      theme: 'dark',
    },
  })
)
