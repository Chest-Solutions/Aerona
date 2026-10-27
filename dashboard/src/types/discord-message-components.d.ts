/**
 * Types for the Discord message renderer the panel borrows for its embed previews.
 *
 * The package ships plain JavaScript - no `.d.ts` of its own and no `@types` on npm - so the props
 * this panel uses are declared here. Each one was read off the library's own README and its built
 * `dist/react.es.js` (the props of the embed are `borderColor`/`embedTitle`, not the older
 * `color`/`title`: the React port renamed them), not guessed. Anything not listed here is a compile
 * error on purpose: a preview that silently ignores a prop is how a wrong colour looks right.
 */
declare module '@discord-message-components/react' {
    import type { FC, ReactNode } from 'react'

    export type DiscordProfile = {
        author?: string
        avatar?: string
        roleColor?: string
        bot?: boolean
    }

    /** The context object `DiscordDefaultOptions` seeds and `DiscordOptionsContext` provides. */
    export const DiscordDefaultOptions: {
        defaultMode: 'cozy' | 'compact'
        defaultTheme: 'light' | 'dark'
        profiles: Record<string, DiscordProfile>
        avatars: Record<string, string>
        resolveImage: (map: Record<string, string>, key?: string) => string | undefined
    }
    export const DiscordOptionsContext: unknown

    export const DiscordMessages: FC<{ children?: ReactNode; compact?: boolean; lightTheme?: boolean }>
    export const DiscordMessage: FC<{
        children?: ReactNode
        author?: string
        avatar?: string
        bot?: boolean
        timestamp?: string
        edited?: boolean
        highlight?: boolean
        roleColor?: string
        profile?: string
        command?: boolean
        interactive?: boolean
    }>
    /** The card. Its text is the children, its heading is `embedTitle`, and Discord tints only the
     *  four pixel bar on the left - which is what `borderColor` is for.
     */
    export const DiscordEmbed: FC<{
        children?: ReactNode
        borderColor?: string
        embedTitle?: string
        url?: string
        authorName?: string
        authorIcon?: string
        authorUrl?: string
        image?: string
        thumbnail?: string
        timestamp?: string
        footerIcon?: string
    }>
    export const DiscordEmbedFields: FC<{ children?: ReactNode }>
    export const DiscordEmbedField: FC<{ children?: ReactNode; fieldTitle?: string; inline?: boolean }>
    export const DiscordMarkdown: FC<{ content?: string; children?: ReactNode; inline?: boolean }>
    export const DiscordMention: FC<{ children?: ReactNode; highlight?: boolean; profile?: string; roleColor?: string }>
    export const DiscordInteraction: FC<{ children?: ReactNode; profile?: string; command?: boolean }>
    export const DiscordButton: FC<{ children?: ReactNode; variant?: 'primary' | 'secondary' | 'success' | 'danger' | 'link'; disabled?: boolean }>
    export const DiscordButtons: FC<{ children?: ReactNode }>
    export const DiscordReaction: FC<{ name?: string; image?: string; count?: number; active?: boolean }>
    export const DiscordReactions: FC<{ children?: ReactNode }>
}

/** The stylesheet, which vite inlines at build time. Declared so the side-effect import resolves. */
declare module '@discord-message-components/react/styles'
