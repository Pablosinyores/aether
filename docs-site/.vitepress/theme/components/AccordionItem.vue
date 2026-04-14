<script setup>
import { ref } from 'vue'

defineProps({
  title: { type: String, required: true },
  icon: { type: String, default: '' },
})

const open = ref(false)
</script>

<template>
  <div class="accordion-item" :class="{ open }">
    <button class="accordion-trigger" @click="open = !open">
      <span class="trigger-content">
        <span v-if="icon" class="trigger-icon">{{ icon }}</span>
        <span class="trigger-title">{{ title }}</span>
      </span>
      <span class="trigger-chevron">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="6 9 12 15 18 9"></polyline>
        </svg>
      </span>
    </button>
    <div class="accordion-body">
      <div class="accordion-content">
        <slot />
      </div>
    </div>
  </div>
</template>

<style scoped>
.accordion-item {
  background: var(--vp-c-bg);
}

.accordion-trigger {
  width: 100%;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 1rem 1.25rem;
  border: none;
  background: none;
  cursor: pointer;
  font-family: var(--vp-font-family-base);
  font-size: 0.95rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
  transition: background 0.2s ease;
}

.accordion-trigger:hover {
  background: var(--vp-c-bg-soft);
}

.trigger-content {
  display: flex;
  align-items: center;
  gap: 0.5rem;
}

.trigger-icon {
  font-size: 1.1rem;
}

.trigger-chevron {
  display: flex;
  align-items: center;
  transition: transform 0.3s cubic-bezier(0.4, 0, 0.2, 1);
  opacity: 0.5;
}

.open .trigger-chevron {
  transform: rotate(180deg);
}

.accordion-body {
  display: grid;
  grid-template-rows: 0fr;
  transition: grid-template-rows 0.35s cubic-bezier(0.4, 0, 0.2, 1);
}

.open .accordion-body {
  grid-template-rows: 1fr;
}

.accordion-content {
  overflow: hidden;
  padding: 0 1.25rem;
}

.open .accordion-content {
  padding: 0 1.25rem 1.25rem;
}

.accordion-content :deep(p) {
  margin: 0.5rem 0;
  font-size: 0.9rem;
  line-height: 1.65;
  opacity: 0.85;
}

.accordion-content :deep(code) {
  font-size: 0.85rem;
}

.accordion-content :deep(ul) {
  margin: 0.5rem 0;
  padding-left: 1.25rem;
}

.accordion-content :deep(li) {
  font-size: 0.9rem;
  line-height: 1.6;
  opacity: 0.85;
}
</style>
