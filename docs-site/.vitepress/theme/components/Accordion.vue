<script setup>
</script>

<template>
  <div class="accordion">
    <slot />
  </div>
</template>

<style scoped>
.accordion {
  border: 1px solid var(--vp-c-bg-soft);
  border-radius: 12px;
  overflow: hidden;
  margin: 1.5rem 0;
}

.accordion :deep(.accordion-item + .accordion-item) {
  border-top: 1px solid var(--vp-c-bg-soft);
}
</style>
