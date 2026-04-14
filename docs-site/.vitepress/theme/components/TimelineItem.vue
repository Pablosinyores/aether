<script setup>
import { ref, onMounted } from 'vue'

defineProps({
  time: { type: String, default: '' },
  title: { type: String, required: true },
  color: { type: String, default: 'brand' },
})

const visible = ref(false)
const item = ref(null)

onMounted(() => {
  const observer = new IntersectionObserver(
    ([entry]) => {
      if (entry.isIntersecting) {
        visible.value = true
        observer.disconnect()
      }
    },
    { threshold: 0.2 }
  )
  if (item.value) observer.observe(item.value)
})
</script>

<template>
  <div
    ref="item"
    class="timeline-item"
    :class="{ visible }"
  >
    <div class="timeline-dot" :class="color" />
    <div class="timeline-card">
      <div class="timeline-header">
        <span v-if="time" class="timeline-time">{{ time }}</span>
        <span class="timeline-title">{{ title }}</span>
      </div>
      <div class="timeline-body">
        <slot />
      </div>
    </div>
  </div>
</template>

<style scoped>
.timeline-item {
  position: relative;
  margin-bottom: 1.5rem;
  opacity: 0;
  transform: translateX(-12px);
  transition: all 0.5s cubic-bezier(0.4, 0, 0.2, 1);
}

.timeline-item.visible {
  opacity: 1;
  transform: translateX(0);
}

.timeline-item:nth-child(1) { transition-delay: 0.05s; }
.timeline-item:nth-child(2) { transition-delay: 0.1s; }
.timeline-item:nth-child(3) { transition-delay: 0.15s; }
.timeline-item:nth-child(4) { transition-delay: 0.2s; }
.timeline-item:nth-child(5) { transition-delay: 0.25s; }
.timeline-item:nth-child(6) { transition-delay: 0.3s; }
.timeline-item:nth-child(7) { transition-delay: 0.35s; }
.timeline-item:nth-child(8) { transition-delay: 0.4s; }

.timeline-dot {
  position: absolute;
  left: -1.65rem;
  top: 0.85rem;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  border: 2px solid var(--vp-c-brand-1);
  background: var(--vp-c-bg);
  z-index: 1;
  transition: all 0.3s ease;
}

.timeline-item.visible .timeline-dot {
  background: var(--vp-c-brand-1);
  box-shadow: 0 0 8px rgba(124, 106, 246, 0.4);
}

.timeline-dot.green {
  border-color: #10b981;
}
.timeline-item.visible .timeline-dot.green {
  background: #10b981;
  box-shadow: 0 0 8px rgba(16, 185, 129, 0.4);
}

.timeline-card {
  background: var(--vp-c-bg-alt);
  border: 1px solid var(--vp-c-bg-soft);
  border-radius: 10px;
  padding: 1rem 1.25rem;
  transition: border-color 0.3s ease;
}

.timeline-item:hover .timeline-card {
  border-color: var(--vp-c-brand-soft);
}

.timeline-header {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  margin-bottom: 0.35rem;
}

.timeline-time {
  font-family: var(--vp-font-family-mono);
  font-size: 0.8rem;
  font-weight: 600;
  color: var(--vp-c-brand-1);
  background: var(--vp-c-brand-soft);
  padding: 0.15rem 0.5rem;
  border-radius: 4px;
  white-space: nowrap;
}

.timeline-title {
  font-weight: 600;
  font-size: 0.95rem;
}

.timeline-body {
  font-size: 0.9rem;
  line-height: 1.6;
  opacity: 0.8;
}

.timeline-body :deep(p) {
  margin: 0.25rem 0;
}

.timeline-body :deep(code) {
  font-size: 0.8rem;
}
</style>
