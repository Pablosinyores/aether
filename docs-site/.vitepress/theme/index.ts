import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import AnimatedCard from './components/AnimatedCard.vue'
import Accordion from './components/Accordion.vue'
import AccordionItem from './components/AccordionItem.vue'
import Timeline from './components/Timeline.vue'
import TimelineItem from './components/TimelineItem.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('AnimatedCard', AnimatedCard)
    app.component('Accordion', Accordion)
    app.component('AccordionItem', AccordionItem)
    app.component('Timeline', Timeline)
    app.component('TimelineItem', TimelineItem)
  },
} satisfies Theme
