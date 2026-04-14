<script setup>
import { ref, onMounted } from 'vue'

defineProps({
  title: String,
  icon: { type: String, default: '' },
})

const visible = ref(false)
const card = ref(null)

onMounted(() => {
  const observer = new IntersectionObserver(
    ([entry]) => {
      if (entry.isIntersecting) {
        visible.value = true
        observer.disconnect()
      }
    },
    { threshold: 0.1 }
  )
  if (card.value) observer.observe(card.value)
})
</script>

<template>
  <div
    ref="card"
    class="animated-card"
    :class="{ visible }"
  >
    <div class="card-glow" />
    <div v-if="icon" class="card-icon">{{ icon }}</div>
    <h3 v-if="title" class="card-title">{{ title }}</h3>
    <div class="card-content">
      <slot />
    </div>
  </div>
</template>

<style scoped>
.animated-card {
  position: relative;
  background: var(--vp-c-bg-alt);
  border: 1px solid var(--vp-c-bg-soft);
  border-radius: 12px;
  padding: 1.5rem;
  margin: 0.75rem 0;
  overflow: hidden;
  opacity: 0;
  transform: translateY(16px);
  transition: all 0.5s cubic-bezier(0.4, 0, 0.2, 1);
}

.animated-card.visible {
  opacity: 1;
  transform: translateY(0);
}

.animated-card:hover {
  border-color: var(--vp-c-brand-soft);
  transform: translateY(-2px);
  box-shadow: 0 8px 32px rgba(124, 106, 246, 0.08);
}

.card-glow {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  height: 2px;
  background: linear-gradient(90deg, transparent, var(--vp-c-brand-1), transparent);
  opacity: 0;
  transition: opacity 0.3s ease;
}

.animated-card:hover .card-glow {
  opacity: 1;
}

.card-icon {
  font-size: 1.5rem;
  margin-bottom: 0.5rem;
}

.card-title {
  font-size: 1.1rem;
  font-weight: 600;
  margin: 0 0 0.5rem;
  letter-spacing: -0.01em;
}

.card-content {
  font-size: 0.95rem;
  line-height: 1.6;
  opacity: 0.85;
}

.card-content :deep(p) {
  margin: 0;
}
</style>
