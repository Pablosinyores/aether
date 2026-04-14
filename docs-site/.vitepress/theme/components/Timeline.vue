<script setup>
</script>

<template>
  <div class="timeline">
    <slot />
  </div>
</template>

<style scoped>
.timeline {
  position: relative;
  margin: 2rem 0;
  padding-left: 2rem;
}

.timeline::before {
  content: '';
  position: absolute;
  left: 7px;
  top: 0;
  bottom: 0;
  width: 2px;
  background: linear-gradient(
    to bottom,
    var(--vp-c-brand-1),
    var(--vp-c-brand-3),
    var(--vp-c-bg-soft)
  );
  border-radius: 1px;
}
</style>
